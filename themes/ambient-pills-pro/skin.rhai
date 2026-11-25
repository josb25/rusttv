// Standalone Ambient Pills Pro script (sandboxed Rhai).
// Up/Down moves between the app dock and the top-right pill bar.

fn on_init() {
    set_int(1, 0); // 0 = app dock, 1 = top-right pill
}

fn on_action(name, arg) {
    if name == "settings" {
        open_settings();
    }
}

fn on_key(key, pressed) {
    if get_int(1) == 1 {
        if !pressed { return true; }
        if key == "Down" || key == "Back" {
            set_int(1, 0);
            return true;
        }
        if key == "Ok" {
            set_int(1, 0);
            open_settings();
            return true;
        }
        if key == "Up" || key == "Left" || key == "Right" {
            return true;
        }
        return false;
    }

    if !pressed { return false; }
    if key == "Up" {
        set_int(1, 1);
        return true;
    }
    false
}
