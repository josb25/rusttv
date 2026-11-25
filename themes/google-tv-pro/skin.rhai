// Level 3 programmable skin script (sandboxed Rhai).
// Drives interactive Google TV tabs, dynamic hero taglines, and shortcuts.

fn tab_name(idx) {
    if idx == 0 { return "For you"; }
    if idx == 1 { return "Movies"; }
    if idx == 2 { return "Shows"; }
    if idx == 3 { return "Apps"; }
    return "Library";
}

fn tab_section(idx) {
    if idx == 0 { return "Your apps"; }
    if idx == 1 { return "Movie apps"; }
    if idx == 2 { return "TV & Series"; }
    if idx == 3 { return "All installed apps"; }
    return "Your library";
}

fn select_tab(idx) {
    let clamped = idx;
    if clamped < 0 { clamped = 4; }
    if clamped > 4 { clamped = 0; }
    set_int(0, clamped);
    set_str(0, tab_name(clamped));
    set_str(2, tab_section(clamped));
}

fn on_init() {
    select_tab(0);
    set_str(1, "Trending  |  Ready to launch on RustTV");
}

fn on_focus(index, id, label) {
    let total = tile_count();
    let pos = index + 1;
    set_str(1, tab_name(get_int(0)) + "  |  " + label + " (" + pos + " of " + total + ")");
}

fn on_action(name, arg) {
    if name == "tab" {
        if arg == "0" { select_tab(0); }
        else if arg == "1" { select_tab(1); }
        else if arg == "2" { select_tab(2); }
        else if arg == "3" { select_tab(3); }
        else if arg == "4" { select_tab(4); }
    } else if name == "settings" {
        open_settings();
    }
}

fn on_key(key, pressed) {
    if !pressed { return false; }
    if key == "q" || key == "[" {
        select_tab(get_int(0) - 1);
        return true;
    }
    if key == "e" || key == "]" {
        select_tab(get_int(0) + 1);
        return true;
    }
    false
}
