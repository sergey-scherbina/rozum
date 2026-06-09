use std::path::PathBuf;

pub fn rooms_dir() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs_home().join(".run"));
    base.join("rozum").join("rooms")
}

pub fn room_socket(name: &str) -> PathBuf {
    rooms_dir().join(format!("{name}.sock"))
}

pub fn ensure_rooms_dir() -> std::io::Result<()> {
    std::fs::create_dir_all(rooms_dir())
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
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
