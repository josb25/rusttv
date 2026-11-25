// Standalone Classic Home Pro script (sandboxed Rhai).
// Rebuilds the classic RustTV 4-column home layout as a 100% peer Level 3 skin.
// State slots:
//   i0 = glow intensity mode (0 = vivid, 1 = subtle — persisted via save_pref)
//   i1 = focus zone: 0 = 4-column app grid, 1 = top-right controls
//   i2 = focused top-right button: 0 = Glow toggle, 1 = Settings
//   i3 = focused app index
//   i4 = top visible row (4 apps per row)

fn on_init() {
    let glow = load_pref("glow", "vivid");
    set_int(0, if glow == "subtle" { 1 } else { 0 });
    set_int(1, 0);
    set_int(2, 1);
    set_int(3, 0);
    set_int(4, 0);
}

fn on_focus(index, id, label) {
    set_int(3, index);
    let row = index / 4;
    let scroll = get_int(4);
    if row < scroll {
        set_int(4, row);
    } else if row >= scroll + 2 {
        set_int(4, row - 1);
    }
}

fn on_action(name, arg) {
    if name == "settings" {
        set_int(1, 0);
        open_settings();
    } else if name == "glow" {
        let next = 1 - get_int(0);
        set_int(0, next);
        save_pref("glow", if next == 1 { "subtle" } else { "vivid" });
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
                let next = 1 - get_int(0);
                set_int(0, next);
                save_pref("glow", if next == 1 { "subtle" } else { "vivid" });
            }
            return true;
        }
        if key == "Up" { return true; }
        return false;
    }

    if !pressed { return false; }
    let cur = get_int(3);
    let max_idx = tile_count() - 1;
    if key == "Left" {
        if cur > 0 { focus(cur - 1); }
        return true;
    }
    if key == "Right" {
        if cur < max_idx { focus(cur + 1); }
        return true;
    }
    if key == "Down" {
        let next = cur + 4;
        if next <= max_idx {
            focus(next);
        } else if (cur / 4) < (max_idx / 4) {
            focus(max_idx);
        }
        return true;
    }
    if key == "Up" {
        if cur >= 4 {
            focus(cur - 4);
            return true;
        }
        set_int(1, 1);
        set_int(2, 1);
        return true;
    }
    false
}
