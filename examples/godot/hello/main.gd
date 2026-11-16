extends Node3D

@onready var cube: MeshInstance3D = $Cube


func _ready() -> void:
	var xr := XRServer.find_interface("OpenXR")
	if xr and xr.is_initialized():
		# The headset paces frames; the desktop window's vsync would halve the rate.
		DisplayServer.window_set_vsync_mode(DisplayServer.VSYNC_DISABLED)
		get_viewport().use_xr = true
		print("Tether hello: XR running")
	else:
		push_error("Tether hello: OpenXR did not initialize")


func _process(delta: float) -> void:
	cube.rotate_y(delta)
