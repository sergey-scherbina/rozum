use std::time::Duration;

use super::room_client::{call_room_tool, tool_result_text_json};
use super::room_path::rooms_dir;

#[derive(Debug)]
pub struct RoomInfo {
    pub name: String,
    pub topic: String,
    pub participants: Vec<String>,
}

/// Scan the rooms directory and ping each socket. Returns live rooms.
pub async fn list_rooms() -> Vec<RoomInfo> {
    let dir = rooms_dir();
    let mut rooms = Vec::new();

    let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
        return rooms;
    };

    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        let Some(name) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.to_owned())
        else {
            continue;
        };
        // Skip symlinks (alias sockets from renames)
        if let Ok(meta) = tokio::fs::symlink_metadata(&path).await {
            if meta.file_type().is_symlink() {
                continue;
            }
        }
        if !path.extension().map_or(false, |e| e == "sock") {
            continue;
        }

        if let Some(info) = ping_room(&name, &path).await {
            rooms.push(info);
        }
    }

    rooms
}

async fn ping_room(name: &str, path: &std::path::Path) -> Option<RoomInfo> {
    let result = call_room_tool(
        path,
        "_room_info",
        serde_json::json!({}),
        "rozum-list",
        Duration::from_secs(2),
    )
    .await
    .ok()?;
    room_info_from_result(name, &result)
}

fn room_info_from_result(default_name: &str, result: &serde_json::Value) -> Option<RoomInfo> {
    if result
        .get("isError")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        return None;
    }

    let payload = tool_result_text_json(result)?;
    let participants = payload
        .get("participants")
        .and_then(serde_json::Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();

    Some(RoomInfo {
        name: payload
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(default_name)
            .to_owned(),
        topic: payload
            .get("topic")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        participants,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_room_info_from_mcp_tool_result_text() {
        let result = serde_json::json!({
            "content": [{
                "type": "text",
                "text": "{\"name\":\"bold-swift\",\"topic\":\"open floor\",\"participants\":[\"sergiy\",\"codex\"]}"
            }],
            "isError": false
        });

        let info = room_info_from_result("fallback", &result).unwrap();

        assert_eq!(info.name, "bold-swift");
        assert_eq!(info.topic, "open floor");
        assert_eq!(info.participants, vec!["sergiy", "codex"]);
    }
}
