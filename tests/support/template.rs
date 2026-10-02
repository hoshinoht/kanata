use std::{fs, path::PathBuf};

pub struct Template(pub PathBuf);

impl Template {
    pub fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "kanata-template-{:016x}",
            getrandom::u64().expect("random id")
        ));
        fs::create_dir(&root).expect("isolated directory");
        fs::create_dir(root.join("keys")).expect("fixture keys directory");
        fs::write(root.join("keys/keys.toml"), "version = 1\nkeys = []\n").expect("fixture keys");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                root.join("keys/keys.toml"),
                fs::Permissions::from_mode(0o600),
            )
            .expect("fixture permissions");
        }
        let path = root.join(name);
        fs::copy(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("config")
                .join(name),
            &path,
        )
        .expect("template copy");
        Self(path)
    }
}

impl Drop for Template {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(self.0.parent().expect("fixture directory"));
    }
}
