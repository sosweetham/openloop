//! Generates the runtime's OpenXR dispatch table from the Khronos registry (`xr.xml`) and the
//! generated ABI (`bindings.rs`). Run through `scripts/openxr-abi.sh`.
//!
//! Signatures come from the `PFN_xr*` aliases in `bindings.rs`, never from the XML, so a stub can
//! only disagree with the header if bindgen does.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::{env, fs, process};

use quote::ToTokens;

const GLOBAL: &[&str] = &[
    "xrEnumerateApiLayerProperties",
    "xrEnumerateInstanceExtensionProperties",
    "xrCreateInstance",
];
const CORE: &[(&str, u64)] = &[("XR_VERSION_1_0", 0), ("XR_VERSION_1_1", 1)];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Requirement {
    Global,
    Core { minor: u64 },
    Extension(usize),
}

struct Extension {
    name: String,
    spec_version: u32,
    commands: Vec<String>,
}

struct Registry {
    aliases: HashMap<String, String>,
    core: Vec<(String, u64)>,
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let [xml, bindings, extension_list, out] = args.as_slice() else {
        eprintln!("usage: tether-openxr-gen <xr.xml> <bindings.rs> <extensions.txt> <out.rs>");
        process::exit(2);
    };

    let xml = read(xml);
    let options = roxmltree::ParsingOptions {
        allow_dtd: true,
        ..Default::default()
    };
    let doc = roxmltree::Document::parse_with_options(&xml, options)
        .unwrap_or_else(|e| fail(&format!("xr.xml: {e}")));
    let bindings =
        syn::parse_file(&read(bindings)).unwrap_or_else(|e| fail(&format!("bindings.rs: {e}")));

    let signatures = pfn_signatures(&bindings);
    let registry = registry(&doc);
    let extensions: Vec<Extension> = read(extension_list)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|name| extension(&doc, name))
        .collect();

    let commands = commands(&registry, &extensions, &signatures);
    let source = render(&commands, &registry, &extensions, &signatures, &bindings);
    let file = syn::parse_file(&source)
        .unwrap_or_else(|e| fail(&format!("generated source does not parse: {e}")));
    fs::write(out, prettyplease::unparse(&file)).unwrap_or_else(|e| fail(&format!("{out}: {e}")));
}

fn read(path: &str) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| fail(&format!("{path}: {e}")))
}

fn fail(message: &str) -> ! {
    eprintln!("tether-openxr-gen: {message}");
    process::exit(1);
}

/// `PFN_xrFoo` → the bare `unsafe extern "C" fn(..)` inside its `Option`.
fn pfn_signatures(bindings: &syn::File) -> HashMap<String, syn::TypeBareFn> {
    bindings
        .items
        .iter()
        .filter_map(|item| match item {
            syn::Item::Type(t) => Some(t),
            _ => None,
        })
        .filter_map(|t| {
            let command = t.ident.to_string().strip_prefix("PFN_")?.to_owned();
            let syn::Type::Path(path) = t.ty.as_ref() else {
                return None;
            };
            let last = path.path.segments.last()?;
            let syn::PathArguments::AngleBracketed(args) = &last.arguments else {
                return None;
            };
            let Some(syn::GenericArgument::Type(syn::Type::BareFn(f))) = args.args.first() else {
                return None;
            };
            Some((command, f.clone()))
        })
        .collect()
}

fn registry(doc: &roxmltree::Document) -> Registry {
    let aliases = doc
        .descendants()
        .filter(|n| n.has_tag_name("command"))
        .filter_map(|n| {
            Some((
                n.attribute("name")?.to_owned(),
                n.attribute("alias")?.to_owned(),
            ))
        })
        .collect();
    let core = doc
        .descendants()
        .filter(|n| n.has_tag_name("feature"))
        .filter_map(|f| {
            let minor = CORE
                .iter()
                .find(|(name, _)| f.attribute("name") == Some(name))?
                .1;
            Some(required_commands(f).into_iter().map(move |c| (c, minor)))
        })
        .flatten()
        .collect();
    Registry { aliases, core }
}

fn required_commands(node: roxmltree::Node) -> Vec<String> {
    node.descendants()
        .filter(|n| {
            n.has_tag_name("command") && n.parent().is_some_and(|p| p.has_tag_name("require"))
        })
        .filter_map(|n| n.attribute("name").map(str::to_owned))
        .collect()
}

fn extension(doc: &roxmltree::Document, name: &str) -> Extension {
    let node = doc
        .descendants()
        .find(|n| n.has_tag_name("extension") && n.attribute("name") == Some(name))
        .unwrap_or_else(|| fail(&format!("extension {name} is not in xr.xml")));
    let spec_version = node
        .descendants()
        .find(|n| {
            n.has_tag_name("enum")
                && n.attribute("name")
                    .is_some_and(|e| e.ends_with("_SPEC_VERSION"))
        })
        .and_then(|n| n.attribute("value")?.parse().ok())
        .unwrap_or_else(|| fail(&format!("extension {name} has no SPEC_VERSION")));
    Extension {
        name: name.to_owned(),
        spec_version,
        commands: required_commands(node),
    }
}

/// Every name the loader may ask for, with what it requires. Core wins over an extension that
/// re-requires a promoted command.
fn commands(
    registry: &Registry,
    extensions: &[Extension],
    signatures: &HashMap<String, syn::TypeBareFn>,
) -> BTreeMap<String, Requirement> {
    let mut out = BTreeMap::new();
    for (index, ext) in extensions.iter().enumerate() {
        for c in &ext.commands {
            out.insert(c.clone(), Requirement::Extension(index));
        }
    }
    for (c, minor) in &registry.core {
        out.insert(c.clone(), Requirement::Core { minor: *minor });
    }
    for c in GLOBAL {
        out.insert((*c).to_owned(), Requirement::Global);
    }
    out.retain(|name, _| {
        let present = signatures.contains_key(target(registry, name));
        if !present {
            eprintln!("tether-openxr-gen: skipping {name}: no PFN in bindings.rs");
        }
        present
    });
    out
}

fn target<'a>(registry: &'a Registry, name: &'a str) -> &'a str {
    registry.aliases.get(name).map_or(name, String::as_str)
}

fn render(
    commands: &BTreeMap<String, Requirement>,
    registry: &Registry,
    extensions: &[Extension],
    signatures: &HashMap<String, syn::TypeBareFn>,
    bindings: &syn::File,
) -> String {
    let fields: BTreeSet<&str> = commands.keys().map(|c| target(registry, c)).collect();
    let mut s = String::new();
    let w = &mut s;

    writeln!(
        w,
        "// @generated by tether-openxr-gen from xr.xml and bindings.rs — do not edit."
    )
    .unwrap();
    writeln!(w, "#![allow(non_snake_case, clippy::all)]").unwrap();
    writeln!(w, "use std::ffi::CStr;").unwrap();
    writeln!(w, "use tether_openxr_abi::*;").unwrap();

    writeln!(
        w,
        "pub struct ExtensionInfo {{ pub name: &'static CStr, pub spec_version: u32 }}"
    )
    .unwrap();
    writeln!(w, "pub static EXTENSIONS: &[ExtensionInfo] = &[").unwrap();
    for e in extensions {
        writeln!(
            w,
            "ExtensionInfo {{ name: c\"{}\", spec_version: {} }},",
            e.name, e.spec_version
        )
        .unwrap();
    }
    writeln!(w, "];").unwrap();

    writeln!(w, "#[derive(Clone, Copy, PartialEq, Eq, Debug)]").unwrap();
    writeln!(
        w,
        "pub enum Requirement {{ Global, Core {{ minor: u64 }}, Extension(usize) }}"
    )
    .unwrap();

    writeln!(w, "pub struct Entries {{").unwrap();
    for f in &fields {
        writeln!(w, "pub {f}: PFN_{f},").unwrap();
    }
    writeln!(w, "}}").unwrap();

    writeln!(w, "impl Entries {{ pub const STUBS: Self = Self {{").unwrap();
    for f in &fields {
        writeln!(w, "{f}: Some(stubs::{f}),").unwrap();
    }
    writeln!(w, "}}; }}").unwrap();

    writeln!(
        w,
        "macro_rules! erase {{ ($f:expr) => {{ \
         // SAFETY: function pointers share one representation; the caller casts back to the \
         // PFN type matching the name it asked for.\n\
         $f.map(|f| unsafe {{ std::mem::transmute::<*const (), unsafe extern \"C\" fn()>(f as *const ()) }}) }} }}"
    )
    .unwrap();
    writeln!(
        w,
        "pub fn lookup(e: &Entries, name: &[u8]) -> Option<(PFN_xrVoidFunction, Requirement)> {{"
    )
    .unwrap();
    writeln!(w, "Some(match name {{").unwrap();
    for (name, req) in commands {
        let req = match req {
            Requirement::Global => "Requirement::Global".to_owned(),
            Requirement::Core { minor } => format!("Requirement::Core {{ minor: {minor} }}"),
            Requirement::Extension(i) => format!("Requirement::Extension({i})"),
        };
        writeln!(
            w,
            "b\"{name}\" => (erase!(e.{}), {req}),",
            target(registry, name)
        )
        .unwrap();
    }
    writeln!(w, "_ => return None, }}) }}").unwrap();

    writeln!(w, "mod stubs {{ use super::*;").unwrap();
    for f in &fields {
        let sig = &signatures[*f];
        let params: Vec<String> = sig
            .inputs
            .iter()
            .map(|a| format!("_: {}", a.ty.to_token_stream()))
            .collect();
        let output = sig.output.to_token_stream().to_string();
        if !output.contains("XrResult") {
            fail(&format!("{f} does not return XrResult"));
        }
        writeln!(
            w,
            "pub unsafe extern \"C\" fn {f}({}) {output} {{ XrResult::XR_ERROR_FUNCTION_UNSUPPORTED }}",
            params.join(", ")
        )
        .unwrap();
    }
    writeln!(w, "}}").unwrap();

    write_names(
        w,
        "result_name",
        "XrResult",
        &enum_values(bindings, "XrResult"),
    );
    write_names(
        w,
        "structure_type_name",
        "XrStructureType",
        &enum_values(bindings, "XrStructureType"),
    );
    s
}

/// The `pub const NAME: T = T(value);` items bindgen emits in `impl T`, in declaration order.
fn enum_values(bindings: &syn::File, ty: &str) -> Vec<(String, i64)> {
    bindings
        .items
        .iter()
        .filter_map(|item| match item {
            syn::Item::Impl(i) if i.self_ty.to_token_stream().to_string() == ty => Some(i),
            _ => None,
        })
        .flat_map(|i| i.items.iter())
        .filter_map(|item| {
            let syn::ImplItem::Const(c) = item else {
                return None;
            };
            let syn::Expr::Call(call) = &c.expr else {
                return None;
            };
            let value = int_value(call.args.first()?)?;
            Some((c.ident.to_string(), value))
        })
        .filter(|(name, _)| !name.ends_with("_MAX_ENUM"))
        .collect()
}

fn int_value(expr: &syn::Expr) -> Option<i64> {
    match expr {
        syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Int(i),
            ..
        }) => i.base10_parse().ok(),
        syn::Expr::Unary(syn::ExprUnary {
            op: syn::UnOp::Neg(_),
            expr,
            ..
        }) => int_value(expr).map(|v| -v),
        _ => None,
    }
}

fn write_names(w: &mut String, func: &str, ty: &str, values: &[(String, i64)]) {
    let mut seen = BTreeSet::new();
    writeln!(
        w,
        "pub fn {func}(v: {ty}) -> Option<&'static CStr> {{ Some(match v.0 {{"
    )
    .unwrap();
    for (name, value) in values {
        if seen.insert(*value) {
            writeln!(w, "{value} => c\"{name}\",").unwrap();
        }
    }
    writeln!(w, "_ => return None, }}) }}").unwrap();
}
