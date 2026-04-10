use std::fs;
use anyhow::Result;
use serde::Deserialize;

#[derive(Deserialize)]
pub struct FileArgs {
    pub path: String
}

pub fn list_files(path: &str) -> Result<String> {
    let entries = fs::read_dir(path)?;

    let mut file_names = Vec::new();
    for entry in entries {
        let entry = entry?;
        let file_name = entry.file_name().into_string().unwrap_or_else(|_| "Unknown".to_string());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            file_names.push(format!("{}/", file_name));
        } else {
            file_names.push(file_name);
        }
    }

    file_names.sort();

    Ok(file_names.join("      "))
}

pub fn read_file(path: &str) -> Result<String> {
    let contents = fs::read_to_string(path)?;
    Ok(contents)
}