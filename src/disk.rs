//! How a jelly id becomes a name on disk.

/// The file or folder name for a jelly id. `project:name` stays as it is except
/// on Windows, which refuses ':' in a path component; there it is `project+name`.
/// `+` is outside the id alphabet (`valid_name`: a-z, 0-9, '-', '_'), so two ids
/// never share a name on disk and the id can be read back from the name.
pub fn name(id: &str) -> String {
    if cfg!(windows) { id.replace(':', "+") } else { id.to_string() }
}

#[cfg(test)]
mod tests {
    use super::name;

    #[test]
    fn project_ids_get_a_disk_name_without_a_colon_on_windows() {
        assert_eq!(name("plan"), "plan");
        assert_eq!(name("p:plan"), if cfg!(windows) { "p+plan" } else { "p:plan" });
    }
}
