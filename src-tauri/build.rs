use std::path::{Path, PathBuf};

fn main() {
    check_version_consistency();
    tauri_build::build()
}

/// 版本号的唯一来源是 `package.json` 的 `version`：
///
/// - `tauri.conf.json` 的 `version` 写成 `"../package.json"`，Tauri 解析配置时会去读它，
///   exe 的 `FileVersion` / `ProductVersion` 和安装包版本都取自这个值；
/// - `Cargo.toml` 里那份由 `npm run version:sync` 同步过来（cargo 要求它是字面量，
///   没法直接引用 package.json）。
///
/// 这里加一道兜底：万一有人只改了 `package.json` 却忘了同步，或者反过来手改了
/// `Cargo.toml`，就让构建直接失败并说清怎么修 —— 否则产出的 exe 版本号会悄悄对不上，
/// 这种问题装到机器上之后很难查。
fn check_version_consistency() {
    let manifest_dir = PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR 未设置"),
    );

    // 让 package.json 改动能触发本脚本重跑，否则改了版本号 cargo 可能不重新编译
    let pkg_path = manifest_dir.join("../package.json");
    println!("cargo:rerun-if-changed={}", pkg_path.display());

    let pkg_version = match read_package_json_version(&pkg_path) {
        Ok(version) => version,
        Err(error) => fail(&format!(
            "无法从 {} 读到版本号：{error}",
            pkg_path.display()
        )),
    };

    let cargo_path = manifest_dir.join("Cargo.toml");
    let cargo_version = match read_cargo_toml_version(&cargo_path) {
        Ok(version) => version,
        Err(error) => fail(&format!("无法从 {} 读到版本号：{error}", cargo_path.display())),
    };

    if pkg_version != cargo_version {
        fail(&format!(
            "版本号不一致：\n  \
                 package.json  = {pkg_version}\n  \
                 Cargo.toml    = {cargo_version}\n\n  \
             版本号只在 package.json 里维护，Cargo.toml 那份是派生出来的。执行：\n\n      \
                 npm run version:sync\n"
        ));
    }
}

/// 取 `package.json` 里第一个 `"version": "..."`。
///
/// package.json 根对象的 `version` 固定在文件最前面，早于 `dependencies`，
/// 所以"第一个"就是根版本号，不需要引入 JSON 解析库。
fn read_package_json_version(path: &Path) -> Result<String, String> {
    let text = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
    let key = "\"version\"";
    let after_key = &text[text.find(key).ok_or("没有 version 字段")? + key.len()..];
    let after_colon = &after_key[after_key.find(':').ok_or("version 字段格式异常")? + 1..];
    let after_quote = &after_colon[after_colon.find('"').ok_or("version 不是字符串")? + 1..];
    let end = after_quote
        .find('"')
        .ok_or("version 字符串没有闭合引号")?;
    Ok(after_quote[..end].to_string())
}

/// 取 `Cargo.toml` 里 `[package]` 段的 `version`。
///
/// 只认 `[package]` 段，避免误取 `[dependencies]` 里某个带 version 的条目。
fn read_cargo_toml_version(path: &Path) -> Result<String, String> {
    let text = std::fs::read_to_string(path).map_err(|error| error.to_string())?;

    let mut in_package = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_package = trimmed == "[package]";
            continue;
        }
        if !in_package {
            continue;
        }
        let Some(rest) = trimmed.strip_prefix("version") else {
            continue;
        };
        let Some(rest) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        let Some(rest) = rest.trim_start().strip_prefix('"') else {
            continue;
        };
        let end = rest.find('"').ok_or("version 字符串没有闭合引号")?;
        return Ok(rest[..end].to_string());
    }

    Err("[package] 段里没有 version 字段".to_string())
}

fn fail(message: &str) -> ! {
    panic!("\n\n[NetSentry 版本号检查未通过]\n{message}\n");
}
