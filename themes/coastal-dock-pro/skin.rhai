// Standalone Coastal Dock Pro script (sandboxed Rhai).
// Press 'm' to toggle between labeled top bar and bare monochrome icons.

fn on_init() {
    set_int(0, 0); // 0 = inline labels, 1 = bare icons
}

fn on_action(name, arg) {
    if name == "settings" {
        open_settings();
    } else if name == "toggle_bar" {
        set_int(0, 1 - get_int(0));
    }
}

fn on_key(key, pressed) {
    if pressed && key == "m" {
        set_int(0, 1 - get_int(0));
        return true;
    }
    false
}
