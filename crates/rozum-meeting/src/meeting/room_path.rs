use std::path::PathBuf;

/// Runtime base for all rozum ephemeral files (room sockets, piggyback drops).
/// `$XDG_RUNTIME_DIR/rozum` or `~/.run/rozum` — tmpfs where available.
pub fn rozum_runtime_dir() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs_home().join(".run"));
    base.join("rozum")
}

pub fn rooms_dir() -> PathBuf {
    rozum_runtime_dir().join("rooms")
}

pub fn room_socket(name: &str) -> PathBuf {
    rooms_dir().join(format!("{name}.sock"))
}

/// The single MCP endpoint of the `rozum meetings` daemon. All rooms are served
/// here; a session selects one via `rooms.join` / `_join_internal`.
pub fn meeting_sock() -> PathBuf {
    rozum_runtime_dir().join("meeting.sock")
}

pub fn ensure_rooms_dir() -> std::io::Result<()> {
    std::fs::create_dir_all(rooms_dir())
}

/// `$HOME`, for callers outside this module that need the same base (the REST secret file).
pub fn dirs_home_public() -> PathBuf {
    dirs_home()
}

fn dirs_home() -> PathBuf {
    rozum_paths::home_dir().unwrap_or_else(rozum_paths::temp_dir)
}

pub fn generate_room_name() -> String {
    let adjectives = [
        "rapid", "bright", "calm", "swift", "bold", "clear", "crisp", "deep", "firm", "free",
        "keen", "kind", "lean", "mild", "neat", "pure", "quick", "rich", "safe", "warm",
    ];
    let nouns = [
        "finch", "robin", "crane", "heron", "swift", "lark", "dove", "wren", "kite", "tern",
        "hawk", "reed", "fern", "moss", "sage", "pine", "oak", "elm", "bay", "glen",
    ];
    use rand::seq::SliceRandom;
    let mut rng = rand::thread_rng();
    let adj = adjectives.choose(&mut rng).unwrap_or(&"rapid");
    let noun = nouns.choose(&mut rng).unwrap_or(&"finch");
    format!("{adj}-{noun}")
}
