extends Node3D

@onready var cube: MeshInstance3D = $Cube
@onready var camera: XRCamera3D = $XROrigin3D/XRCamera3D
@onready var left_hand: XRController3D = $XROrigin3D/LeftHand
@onready var right_hand: XRController3D = $XROrigin3D/RightHand

var pause_label: Label3D
## Paused with the menu button, as opposed to by a system overlay taking input focus.
var paused_by_player := false


func _ready() -> void:
	var xr := XRServer.find_interface("OpenXR")
	if xr and xr.is_initialized():
		# The headset paces frames; the desktop window's vsync would halve the rate.
		DisplayServer.window_set_vsync_mode(DisplayServer.VSYNC_DISABLED)
		get_viewport().use_xr = true
		# STAGE never moves on a system recenter, so the game re-centres itself.
		xr.pose_recentered.connect(func(): XRServer.center_on_hmd(XRServer.RESET_BUT_KEEP_TILT, true))
		# A system overlay (Tether's menu) has the input: pause and let it draw the controllers.
		xr.session_visible.connect(_on_focus_lost)
		xr.session_focussed.connect(_on_focus_regained)
		print("Tether hello: XR running")
	else:
		push_error("Tether hello: OpenXR did not initialize")

	# Tracking and the pause toggle keep running while the game is paused.
	process_mode = Node.PROCESS_MODE_ALWAYS
	cube.process_mode = Node.PROCESS_MODE_PAUSABLE
	pause_label = Label3D.new()
	pause_label.text = "Paused\nPress ☰ to resume"
	pause_label.font_size = 96
	pause_label.pixel_size = 0.001
	pause_label.outline_size = 12
	pause_label.position = Vector3(0, 0, -1.2)
	pause_label.no_depth_test = true
	pause_label.visible = false
	camera.add_child(pause_label)
	left_hand.button_pressed.connect(_on_left_button)


func _on_left_button(action: String) -> void:
	if action == "menu_button":
		paused_by_player = not paused_by_player
		_set_paused(paused_by_player)


func _on_focus_lost() -> void:
	left_hand.visible = false
	right_hand.visible = false
	_set_paused(true)


func _on_focus_regained() -> void:
	left_hand.visible = true
	right_hand.visible = true
	_set_paused(paused_by_player)


func _set_paused(paused: bool) -> void:
	get_tree().paused = paused
	pause_label.visible = paused


func _process(delta: float) -> void:
	if not get_tree().paused:
		cube.rotate_y(delta)
