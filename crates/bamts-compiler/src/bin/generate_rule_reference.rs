use std::{fs, path::PathBuf};

fn main() -> Result<(), std::io::Error> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("RULES.md");
    fs::write(path, bamts_compiler::lint::rule_reference())
}
