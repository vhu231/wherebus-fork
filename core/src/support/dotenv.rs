//! 极简 .env 读取：启动时把文件里的键值填进进程环境变量。
//!
//! 规则与常见 dotenv 实现一致：`#` 开头是注释，允许 `export` 前缀，
//! 值可以用单/双引号包起来（双引号里支持 `\n` `\t` `\\` `\"` 转义），
//! **已经存在的环境变量不会被覆盖**——真实环境优先于文件。
use std::path::Path;

/// 解析 .env 文本，返回键值对（保持文件里的先后顺序）。
pub fn parse(content: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for line in content.lines() {
        let line = line.trim_start_matches('\u{feff}').trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        pairs.push((key.to_string(), unquote(value.trim())));
    }
    pairs
}

fn unquote(raw: &str) -> String {
    let bytes = raw.as_bytes();
    match (bytes.first(), bytes.last()) {
        (Some(b'"'), Some(b'"')) if raw.len() >= 2 => unescape(&raw[1..raw.len() - 1]),
        (Some(b'\''), Some(b'\'')) if raw.len() >= 2 => raw[1..raw.len() - 1].to_string(),
        // 未加引号的值：空白 + # 之后视为行尾注释
        _ => match raw.split_once(" #") {
            Some((value, _)) => value.trim_end().to_string(),
            None => raw.to_string(),
        },
    }
}

fn unescape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// 载入 .env；文件不存在就什么都不做。返回实际写入的变量名。
pub fn load(path: impl AsRef<Path>) -> Vec<String> {
    let path = path.as_ref();
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut applied = Vec::new();
    for (key, value) in parse(&content) {
        // 真实环境变量优先，便于用 `VAR=... cargo run` 临时覆盖
        if std::env::var_os(&key).is_none() {
            // SAFETY: 只在启动早期、开始创建线程之前调用
            unsafe { std::env::set_var(&key, &value) };
            applied.push(key);
        }
    }
    applied
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_comments_quotes_and_export() {
        let content = "\u{feff}# 注释\n\
            \n\
            TELEGRAM_BOT_TOKEN=123:ABC\n\
            export WHEREBUS_BIND=0.0.0.0:8080\n\
            QUOTED=\"带 空格 的值\"\n\
            SINGLE='原样 $保留'\n\
            ESCAPED=\"第一行\\n第二行\"\n\
            TRAILING=值 # 行尾注释\n\
            HASH_IN_VALUE=abc#def\n\
            EMPTY=\n\
            不合法的行\n\
            带空格的键 = 值\n";
        let pairs = parse(content);
        assert_eq!(
            pairs,
            vec![
                ("TELEGRAM_BOT_TOKEN".into(), "123:ABC".into()),
                ("WHEREBUS_BIND".into(), "0.0.0.0:8080".into()),
                ("QUOTED".into(), "带 空格 的值".into()),
                ("SINGLE".into(), "原样 $保留".into()),
                ("ESCAPED".into(), "第一行\n第二行".into()),
                ("TRAILING".into(), "值".into()),
                // 没有空格分隔的 # 属于值的一部分
                ("HASH_IN_VALUE".into(), "abc#def".into()),
                ("EMPTY".into(), String::new()),
            ]
        );
    }

    #[test]
    fn existing_environment_wins_and_missing_file_is_fine() {
        assert!(load("绝对不存在的文件.env").is_empty());

        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "wherebus-dotenv-test-{}.env",
            std::process::id()
        ));
        std::fs::write(&path, "WHEREBUS_TEST_NEW=来自文件\nWHEREBUS_TEST_SET=来自文件\n").unwrap();
        // SAFETY: 测试内单线程设置
        unsafe { std::env::set_var("WHEREBUS_TEST_SET", "来自环境") };

        let applied = load(&path);
        assert_eq!(applied, vec!["WHEREBUS_TEST_NEW".to_string()]);
        assert_eq!(std::env::var("WHEREBUS_TEST_NEW").unwrap(), "来自文件");
        assert_eq!(std::env::var("WHEREBUS_TEST_SET").unwrap(), "来自环境");

        unsafe {
            std::env::remove_var("WHEREBUS_TEST_NEW");
            std::env::remove_var("WHEREBUS_TEST_SET");
        }
        let _ = std::fs::remove_file(&path);
    }
}
