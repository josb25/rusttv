// Standalone Coastal Dock Pro script (sandboxed Rhai).
// Up/Down moves between the bottom dock and top-right bar; 'm' toggles bar style.

fn on_init() {
    set_int(0, 0); // 0 = inline labels, 1 = bare icons
    set_int(1, 0); // 0 = bottom dock, 1 = top bar
    set_int(2, 1); // 0 = bar style toggle, 1 = Settings
}

fn on_action(name, arg) {
    if name == "settings" {
        open_settings();
    } else if name == "toggle_bar" {
        set_int(0, 1 - get_int(0));
    }
}

fn on_key(key, pressed) {
    if get_int(1) == 1 {
        if !pressed { return true; }
        let cur = get_int(2);
        if key == "Down" || key == "Back" {
            set_int(1, 0);
            return true;
        }
        if key == "Left" || key == "Right" {
            set_int(2, 1 - cur);
            return true;
        }
        if key == "Ok" {
            if cur == 1 {
                set_int(1, 0);
                open_settings();
            } else {
                set_int(0, 1 - get_int(0));
            }
            return true;
        }
        if key == "Up" { return true; }
        return false;
    }

    if !pressed { return false; }
    if key == "Up" {
        set_int(1, 1);
        set_int(2, 1);
        return true;
    }
    if key == "m" {
        set_int(0, 1 - get_int(0));
        return true;
    }
    false
}
