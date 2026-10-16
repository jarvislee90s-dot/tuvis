// JSONC 无损编辑：基于文本 span 定位键值，保留注释与既有格式
// 仅支持「顶层对象 → 对象型 section → 任意值 entry」两级结构（覆盖 mcp / plugins 场景）

#[derive(Debug, Clone, Copy, PartialEq)]
struct Span {
    start: usize, // 字节下标（含）
    end: usize,   // 字节下标（不含）
}

struct Cursor<'a> {
    s: &'a str,
    b: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(s: &'a str) -> Self {
        Cursor {
            s,
            b: s.as_bytes(),
            pos: 0,
        }
    }

    /// 跳过空白与注释
    fn skip_trivia(&mut self) {
        loop {
            while self.pos < self.b.len()
                && matches!(self.b[self.pos], b' ' | b'\t' | b'\r' | b'\n')
            {
                self.pos += 1;
            }
            if self.pos + 1 < self.b.len() && self.b[self.pos] == b'/' {
                if self.b[self.pos + 1] == b'/' {
                    while self.pos < self.b.len() && self.b[self.pos] != b'\n' {
                        self.pos += 1;
                    }
                    continue;
                }
                if self.b[self.pos + 1] == b'*' {
                    self.pos += 2;
                    while self.pos + 1 < self.b.len()
                        && !(self.b[self.pos] == b'*' && self.b[self.pos + 1] == b'/')
                    {
                        self.pos += 1;
                    }
                    self.pos = (self.pos + 2).min(self.b.len());
                    continue;
                }
            }
            break;
        }
    }

    /// 读取字符串字面量（pos 位于 '"'），返回内容并推进到引号后
    fn read_string(&mut self) -> Option<String> {
        if self.pos >= self.b.len() || self.b[self.pos] != b'"' {
            return None;
        }
        self.pos += 1;
        let mut out = String::new();
        while self.pos < self.b.len() {
            match self.b[self.pos] {
                b'\\' => {
                    self.pos += 1;
                    if self.pos < self.b.len() {
                        out.push(self.b[self.pos] as char);
                        self.pos += 1;
                    }
                }
                b'"' => {
                    self.pos += 1;
                    return Some(out);
                }
                c if c < 0x80 => {
                    out.push(c as char);
                    self.pos += 1;
                }
                _ => {
                    let ch = self.s[self.pos..].chars().next()?;
                    out.push(ch);
                    self.pos += ch.len_utf8();
                }
            }
        }
        None
    }

    /// 跳过一个值，返回其 span（已去除前导 trivia 与尾随空白）
    fn skip_value(&mut self) -> Option<Span> {
        self.skip_trivia();
        let value_start = self.pos;
        if self.pos >= self.b.len() {
            return None;
        }
        match self.b[self.pos] {
            b'"' => {
                self.read_string()?;
            }
            b'{' | b'[' => {
                let open = self.b[self.pos];
                let close = if open == b'{' { b'}' } else { b']' };
                let mut depth = 0usize;
                loop {
                    self.skip_trivia();
                    if self.pos >= self.b.len() {
                        return None;
                    }
                    let c = self.b[self.pos];
                    if c == b'"' {
                        self.read_string()?;
                        continue;
                    }
                    if c == open {
                        depth += 1;
                        self.pos += 1;
                        continue;
                    }
                    if c == close {
                        depth -= 1;
                        self.pos += 1;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        self.pos += 1;
                    }
                }
            }
            _ => {
                while self.pos < self.b.len() {
                    let c = self.b[self.pos];
                    if c == b',' || c == b'}' || c == b']' {
                        break;
                    }
                    if c == b'/'
                        && self.pos + 1 < self.b.len()
                        && (self.b[self.pos + 1] == b'/' || self.b[self.pos + 1] == b'*')
                    {
                        break;
                    }
                    self.pos += 1;
                }
                while self.pos > value_start
                    && matches!(self.b[self.pos - 1], b' ' | b'\t' | b'\r' | b'\n')
                {
                    self.pos -= 1;
                }
            }
        }
        Some(Span {
            start: value_start,
            end: self.pos,
        })
    }
}

struct Entry {
    key: String,
    key_start: usize,
    value: Span,
}

/// 遍历对象（cur.pos 位于 '{'），返回 (对象闭合 span, entries)
fn walk_object(cur: &mut Cursor) -> Option<(Span, Vec<Entry>)> {
    let obj_start = cur.pos;
    cur.pos += 1; // 跳过 '{'
    let mut entries = Vec::new();
    loop {
        cur.skip_trivia();
        if cur.pos >= cur.b.len() {
            return None;
        }
        if cur.b[cur.pos] == b'}' {
            cur.pos += 1;
            break;
        }
        let key_start = cur.pos;
        let key = cur.read_string()?;
        cur.skip_trivia();
        if cur.pos >= cur.b.len() || cur.b[cur.pos] != b':' {
            return None;
        }
        cur.pos += 1;
        let value = cur.skip_value()?;
        entries.push(Entry {
            key,
            key_start,
            value,
        });
        cur.skip_trivia();
        if cur.pos < cur.b.len() && cur.b[cur.pos] == b',' {
            cur.pos += 1;
        }
    }
    Some((
        Span {
            start: obj_start,
            end: cur.pos,
        },
        entries,
    ))
}

fn parse_root(content: &str) -> Result<(Span, Vec<Entry>), String> {
    let mut cur = Cursor::new(content);
    cur.skip_trivia();
    if cur.pos >= cur.b.len() || cur.b[cur.pos] != b'{' {
        return Err("MCPC_EDIT_ROOT: 无法安全编辑该 JSONC 文件：根节点不是对象".to_string());
    }
    walk_object(&mut cur).ok_or_else(|| "无法安全编辑该 JSONC 文件：对象结构不完整".to_string())
}

/// 在顶层对象的 section 中插入/覆盖 entry（值为紧凑 JSON 字符串）
pub fn upsert_entry(
    content: &str,
    section: &str,
    key: &str,
    value_json: &str,
) -> Result<String, String> {
    let (root, root_entries) = parse_root(content)?;
    let section_entry = root_entries.iter().find(|e| e.key == section);
    let mut out = String::new();
    match section_entry {
        Some(se) => {
            let mut cur = Cursor::new(content);
            cur.pos = se.value.start;
            if cur.b[cur.pos] != b'{' {
                return Err(format!(
                    "MCPC_EDIT_SECTION_NOT_OBJ: 无法安全编辑：section \"{}\" 不是对象",
                    section
                ));
            }
            let (_, entries) = walk_object(&mut cur).ok_or_else(|| {
                format!(
                    "MCPC_EDIT_SECTION_INCOMPLETE: 无法安全编辑：section \"{}\" 结构不完整",
                    section
                )
            })?;
            if let Some(existing) = entries.iter().find(|e| e.key == key) {
                // 覆盖：只替换 value span
                out.push_str(&content[..existing.value.start]);
                out.push_str(value_json);
                out.push_str(&content[existing.value.end..]);
                return Ok(out);
            }
            // 插入到 section 收尾 '}' 之前
            let close = se.value.end - 1;
            out.push_str(&content[..close]);
            if !entries.is_empty() {
                out.push(',');
            }
            out.push_str(&format!(" \"{}\": {} ", key, value_json));
            out.push_str(&content[close..]);
        }
        None => {
            // section 不存在：插到 root 收尾 '}' 之前
            let close = root.end - 1;
            out.push_str(&content[..close]);
            if !root_entries.is_empty() {
                out.push(',');
            }
            out.push_str(&format!(
                " \"{}\": {{\"{}\": {}}} ",
                section, key, value_json
            ));
            out.push_str(&content[close..]);
        }
    }
    Ok(out)
}

/// 从顶层对象的 section 中删除 entry；section/entry 不存在视为成功（幂等）
pub fn remove_entry(content: &str, section: &str, key: &str) -> Result<String, String> {
    let (_, root_entries) = parse_root(content)?;
    let Some(se) = root_entries.iter().find(|e| e.key == section) else {
        return Ok(content.to_string());
    };
    let mut cur = Cursor::new(content);
    cur.pos = se.value.start;
    let Some((_, entries)) = walk_object(&mut cur) else {
        return Err(format!(
            "MCPC_EDIT_SECTION_INCOMPLETE: 无法安全编辑：section \"{}\" 结构不完整",
            section
        ));
    };
    let Some(target) = entries.iter().find(|e| e.key == key) else {
        return Ok(content.to_string());
    };

    // 删除范围：entry key 前（含前导逗号）到 value 结束（含后导逗号）
    let mut remove_start = target.key_start;
    let mut remove_end = target.value.end;
    let has_next = entries.iter().any(|e| e.key_start >= target.value.end);
    // 向左吃掉逗号 + 紧邻空白
    let mut i = remove_start;
    while i > se.value.start {
        let c = content.as_bytes()[i - 1];
        if c == b' ' || c == b'\t' || c == b'\r' || c == b'\n' {
            i -= 1;
        } else {
            break;
        }
    }
    if content.as_bytes()[i - 1] == b',' {
        remove_start = i - 1;
    } else if has_next {
        // 首个 entry 且后面还有：向右吃掉逗号
        let mut j = remove_end;
        let bytes = content.as_bytes();
        while j < se.value.end
            && (bytes[j] == b' ' || bytes[j] == b'\t' || bytes[j] == b'\r' || bytes[j] == b'\n')
        {
            j += 1;
        }
        if j < se.value.end && bytes[j] == b',' {
            remove_end = j + 1;
        }
    }
    let mut out = String::new();
    out.push_str(&content[..remove_start]);
    out.push_str(&content[remove_end..]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
  // top comment
  "$schema": "https://x.dev/schema.json",
  "mcp": {
    /* block comment */
    "old": { "type": "local", "command": ["a"] }
  },
  "plugins": {
    "p1": true
  }
}
"#;

    #[test]
    fn upsert_new_entry_keeps_comments() {
        let out =
            upsert_entry(SAMPLE, "mcp", "new", r#"{"type":"local","command":["b"]}"#).unwrap();
        assert!(out.contains("// top comment"));
        assert!(out.contains("/* block comment */"));
        assert!(out.contains("\"new\": {\"type\":\"local\",\"command\":[\"b\"]}"));
        assert!(out.contains("\"old\""));
        // 结果仍是合法 JSON（测试样本无注释干扰新段）
        let json_only = out
            .replace("// top comment", "")
            .replace("/* block comment */", "");
        let v: serde_json::Value = serde_json::from_str(&json_only).unwrap();
        assert_eq!(v["mcp"]["new"]["command"][0], "b");
    }

    #[test]
    fn upsert_replaces_existing_value_only() {
        let out = upsert_entry(SAMPLE, "mcp", "old", r#"{"x":1}"#).unwrap();
        assert!(out.contains("\"old\": {\"x\":1}"));
        assert!(!out.contains("\"command\": [\"a\"]"));
        assert!(out.contains("// top comment"));
    }

    #[test]
    fn remove_entry_deletes_key_and_comma() {
        let out = remove_entry(SAMPLE, "mcp", "old").unwrap();
        assert!(!out.contains("\"old\""));
        assert!(out.contains("\"mcp\""));
        assert!(out.contains("// top comment"));
    }

    #[test]
    fn remove_missing_entry_is_noop() {
        let out = remove_entry(SAMPLE, "mcp", "ghost").unwrap();
        assert_eq!(out, SAMPLE);
    }

    #[test]
    fn section_missing_creates_section_on_upsert() {
        let out = upsert_entry("{\n  \"a\": 1\n}\n", "mcp", "n", "{}").unwrap();
        assert!(out.contains("\"mcp\": {\"n\": {}}"));
        assert!(out.contains("\"a\": 1"));
    }

    #[test]
    fn non_object_root_is_rejected() {
        assert!(upsert_entry("[1,2]", "mcp", "n", "{}").is_err());
    }

    /// 删除 `//` 与 `/* */` 注释，得到可被 serde_json 解析的纯 JSON
    fn strip_comments(s: &str) -> String {
        let mut out = String::new();
        let bytes = s.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'/' {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
                continue;
            }
            out.push(bytes[i] as char);
            i += 1;
        }
        out
    }

    // 3 个 entry 的 section，含注释
    const THREE: &str = r#"{
  "mcp": {
    // keep one
    "srv1": { "port": 1 },
    // vanish me
    "srv2": { "port": 2 },
    "srv3": { "port": 3 }
  },
  "other": 1
}
"#;

    #[test]
    fn remove_middle_entry_absorbs_left_comma_keeps_comments() {
        let out = remove_entry(THREE, "mcp", "srv2").unwrap();
        assert!(out.contains("\"srv1\""));
        assert!(out.contains("\"srv3\""));
        assert!(!out.contains("\"srv2\""));
        assert!(!out.contains(",,"));
        // 注释全部保留
        assert!(out.contains("// keep one"));
        assert!(out.contains("// vanish me"));
        let v: serde_json::Value = serde_json::from_str(&strip_comments(&out)).unwrap();
        assert_eq!(v["mcp"]["srv1"]["port"], 1);
        assert_eq!(v["mcp"]["srv3"]["port"], 3);
    }

    #[test]
    fn remove_first_entry_absorbs_comma_rightward() {
        let out = remove_entry(THREE, "mcp", "srv1").unwrap();
        assert!(out.contains("\"srv2\""));
        assert!(out.contains("\"srv3\""));
        assert!(!out.contains("\"srv1\""));
        assert!(!out.contains(",,"));
        // 注释全部保留
        assert!(out.contains("// keep one"));
        assert!(out.contains("// vanish me"));
        let v: serde_json::Value = serde_json::from_str(&strip_comments(&out)).unwrap();
        assert_eq!(v["mcp"]["srv2"]["port"], 2);
        assert_eq!(v["mcp"]["srv3"]["port"], 3);
    }

    #[test]
    fn remove_last_entry_absorbs_comma_leftward() {
        let out = remove_entry(THREE, "mcp", "srv3").unwrap();
        assert!(out.contains("\"srv1\""));
        assert!(out.contains("\"srv2\""));
        assert!(!out.contains("\"srv3\""));
        assert!(!out.contains(",,"));
        assert!(out.contains("// keep one"));
        let v: serde_json::Value = serde_json::from_str(&strip_comments(&out)).unwrap();
        assert_eq!(v["mcp"]["srv1"]["port"], 1);
        assert_eq!(v["mcp"]["srv2"]["port"], 2);
    }

    #[test]
    fn remove_last_entry_leaves_empty_object() {
        const SINGLE: &str = r#"{
  "mcp": {
    "only": true
  },
  "other": 1
}
"#;
        let out = remove_entry(SINGLE, "mcp", "only").unwrap();
        assert!(!out.contains("\"only\""));
        assert!(!out.contains(",,"));
        // 空 section 仍为合法 JSON 对象
        assert!(out.contains("\"mcp\""));
        let v: serde_json::Value = serde_json::from_str(&strip_comments(&out)).unwrap();
        assert_eq!(v["mcp"], serde_json::Value::Object(Default::default()));
        assert_eq!(v["other"], 1);
    }
}
