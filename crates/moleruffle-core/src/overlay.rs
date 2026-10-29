//! 覆盖件指纹(2016 联机版)。
//!
//! 私服把改过的 SWF / XML(覆盖件)放在与原版相同的网址上,并发布一张指纹表
//! `mo-ov.json` = `{请求路径: 内容 sha1 前 8 位}`。网页版给命中的请求追加 `mo_ov=<指纹>`,
//! 内容一变网址就变(MoleRosetta/deploy/browser/index.html 的 `__moOvUrl`)。
//!
//! 我们的磁盘缓存按完整网址存,不带指纹就会一直用旧覆盖件,所以照做:启动时取一次指纹表,
//! 之后发往本服的请求按表追加指纹。表还没到时,这些请求不读也不写缓存(见 `pending`)。

use std::collections::HashMap;
use std::sync::OnceLock;

use url::Url;

use crate::server;

static TABLE: OnceLock<HashMap<String, String>> = OnceLock::new();

/// 装入指纹表(只认第一次)。
pub fn install(json: &[u8]) -> Result<usize, String> {
    let table: HashMap<String, String> = serde_json::from_slice(json).map_err(|e| e.to_string())?;
    let n = table.len();
    let _ = TABLE.set(table);
    Ok(n)
}

/// 当前服务器有指纹表、但还没取到。
pub fn pending() -> bool {
    server::selected().overlay_index.is_some() && TABLE.get().is_none()
}

/// 按指纹表改写网址;不在表里、已经带了指纹、或主 SWF(每次都取新的,不缓存)时返回 `None`。
pub fn rewrite(abs: &Url) -> Option<String> {
    let table = TABLE.get()?;
    let base = Url::parse(server::selected().base_url).ok()?;
    rewrite_with(table, &base, abs)
}

fn rewrite_with(table: &HashMap<String, String>, base: &Url, abs: &Url) -> Option<String> {
    if abs.origin() != base.origin() {
        return None;
    }
    if abs
        .query_pairs()
        .any(|(k, _)| k == "mo_ov")
    {
        return None;
    }
    // 与网页版同口径:去掉本服目录前缀、连续斜杠并成一个、百分号编码解开。
    let rest = abs.path().strip_prefix(base.path())?;
    let mut path = String::with_capacity(rest.len() + 1);
    for c in format!("/{rest}").chars() {
        if !(c == '/' && path.ends_with('/')) {
            path.push(c);
        }
    }
    let path = percent_decode(&path);
    if path.eq_ignore_ascii_case("/Client.swf") {
        return None;
    }
    let hash = table.get(&path)?;
    if hash.len() != 8 || !hash.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return None;
    }
    // 直接拼在原网址后面,不重排已有的查询串(版本串一个字不动)。
    let mut out = abs.clone();
    out.set_fragment(None);
    let mut s = out.to_string();
    s.push(if abs.query().is_some() { '&' } else { '?' });
    s.push_str("mo_ov=");
    s.push_str(hash);
    if let Some(frag) = abs.fragment() {
        s.push('#');
        s.push_str(frag);
    }
    Some(s)
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(b);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> HashMap<String, String> {
        serde_json::from_str(
            r#"{"/Client.swf":"eca13e9a","/dll/ClientAppDLL.swf":"a18d1d7c","/resource/map/季节.swf":"0123abcd"}"#,
        )
        .unwrap()
    }

    #[test]
    fn 命中的网址追加指纹_其余不动() {
        let base = Url::parse("https://moleworld.net/mole2016/").unwrap();
        let t = table();
        let u = |s: &str| Url::parse(s).unwrap();
        assert_eq!(
            rewrite_with(&t, &base, &u("https://moleworld.net/mole2016/dll/ClientAppDLL.swf?igkdz5ps")).as_deref(),
            Some("https://moleworld.net/mole2016/dll/ClientAppDLL.swf?igkdz5ps&mo_ov=a18d1d7c")
        );
        assert_eq!(
            rewrite_with(&t, &base, &u("https://moleworld.net/mole2016//dll/ClientAppDLL.swf")).as_deref(),
            Some("https://moleworld.net/mole2016//dll/ClientAppDLL.swf?mo_ov=a18d1d7c")
        );
        assert!(
            rewrite_with(&t, &base, &u("https://moleworld.net/mole2016/resource/map/%E5%AD%A3%E8%8A%82.swf"))
                .is_some(),
            "中文文件名按解码后的路径比对"
        );
        assert!(rewrite_with(&t, &base, &u("https://moleworld.net/mole2016/Client.swf")).is_none());
        assert!(
            rewrite_with(&t, &base, &u("https://moleworld.net/mole2016/dll/ClientAppDLL.swf?mo_ov=1")).is_none()
        );
        assert!(rewrite_with(&t, &base, &u("https://moleworld.net/other/dll/ClientAppDLL.swf")).is_none());
        assert!(rewrite_with(&t, &base, &u("https://cdn.example/mole2016/dll/ClientAppDLL.swf")).is_none());
    }
}
