use std::path::Path;

use crate::protocol::ResultKind;

/// Map a file extension to its ResultKind.
pub fn kind_from_path(path: &Path) -> ResultKind {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    match ext.as_str() {
        // Applications
        "app" | "appex" | "prefpane" | "plugin" | "bundle" => ResultKind::Application,

        // Images
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "tiff" | "tif" | "webp"
        | "heic" | "heif" | "svg" | "ico" | "icns" | "raw" | "cr2" | "cr3"
        | "nef" | "arw" | "raf" | "dng" | "orf" | "rw2" | "pef" | "x3f" => {
            ResultKind::Image
        }

        // Videos
        "mp4" | "mov" | "avi" | "mkv" | "wmv" | "flv" | "m4v" | "3gp"
        | "mpeg" | "mpg" | "webm" => ResultKind::Video,

        // Audio
        "mp3" | "aac" | "wav" | "aiff" | "flac" | "ogg" | "m4a" | "wma"
        | "opus" | "mid" | "midi" => ResultKind::Audio,

        // Documents
        "pdf" | "doc" | "docx" | "rtf" | "txt" | "md" | "pages" | "numbers"
        | "keynote" | "epub" | "mobi" | "csv" | "xls" | "xlsx" | "ppt"
        | "pptx" | "ods" | "odt" => ResultKind::Document,

        // Archives
        "zip" | "rar" | "7z" | "tar" | "gz" | "bz2" | "xz" | "dmg"
        | "pkg" | "deb" | "rpm" | "iso" => ResultKind::Archive,

        // Code
        "swift" | "rs" | "go" | "py" | "js" | "ts" | "jsx" | "tsx"
        | "java" | "kt" | "scala" | "cpp" | "c" | "h" | "hpp" | "cc"
        | "cs" | "php" | "rb" | "pl" | "sh" | "zsh" | "fish" | "ps1"
        | "sql" | "html" | "htm" | "css" | "scss" | "sass" | "less"
        | "json" | "xml" | "yaml" | "yml" | "toml" | "dockerfile"
        | "makefile" | "cmake" | "gradle" | "lua" | "r" | "m" | "mm"
        | "dart" | "groovy" | "clj" | "elm" | "ex" | "exs" | "erl"
        | "fs" | "fsx" | "hs" | "jl" | "nim" | "pas" | "proto"
        | "purs" | "sb3" | "sol" | "v" | "vue" | "zig" | "tf" => {
            ResultKind::Code
        }

        // Default
        _ => ResultKind::File,
    }
}

/// Check if a path is an app bundle (macOS .app directory).
pub fn is_app_bundle(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e == "app")
        .unwrap_or(false)
        && path.is_dir()
}
