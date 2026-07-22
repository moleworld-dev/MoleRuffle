//! MoleRuffle:安卓 UI 后端。
//!
//! 官方 ruffle-android 用 `NullUiBackend`,其 `load_device_font` 是空实现 —— 摩尔庄园 SWF
//! 请求的设备字体(SimSun/Arial/Noto Sans/Tahoma/Times New Roman)全部落空,中文渲染不出来。
//!
//! 这里实现一个 catch-all 后端:对**任何**字体请求都返回设备自带的 CJK 字体
//! (`/system/fonts/SysSans-Hans-Regular.ttf` 等),不打包进 APK。其余方法照抄 NullUiBackend 的 no-op。

use std::sync::Arc;

use ruffle_core::backend::ui::{
    DialogResultFuture, FileDialogResult, FileFilter, FontDefinition, FullscreenError,
    LanguageIdentifier, MouseCursor, MultiDialogResultFuture, MultiFileDialogResult, UiBackend,
    US_ENGLISH,
};
use ruffle_core::font::{FontFileData, FontQuery};
use url::Url;

/// 优先用纯 TTF(好解析);TTC 放后面(index 0 未必是简中 face)。
const CJK_FONT_CANDIDATES: &[&str] = &[
    "/system/fonts/SysSans-Hans-Regular.ttf",
    "/system/fonts/SysFont-Hans-Regular.ttf",
    "/system/fonts/DroidSansFallback.ttf",
    "/system/fonts/DroidSansChinese.ttf",
    "/system/fonts/NotoSansCJK-Regular.ttc",
];

fn load_cjk_font() -> Vec<u8> {
    for path in CJK_FONT_CANDIDATES {
        match std::fs::read(path) {
            Ok(data) if !data.is_empty() => {
                log::warn!(
                    "MoleRuffle: 加载系统 CJK 字体 {path}({} KB),用作缺失设备字体的回退",
                    data.len() / 1024
                );
                return data;
            }
            _ => {}
        }
    }
    log::error!("MoleRuffle: 未找到任何系统 CJK 字体,游戏内中文可能缺字");
    Vec::new()
}

pub struct AndroidUiBackend {
    /// 设备 CJK 字体字节,`Arc` 共享给每个字体请求(零拷贝)。
    cjk_font: Arc<Vec<u8>>,
}

impl AndroidUiBackend {
    pub fn new() -> Self {
        Self {
            cjk_font: Arc::new(load_cjk_font()),
        }
    }
}

impl Default for AndroidUiBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl UiBackend for AndroidUiBackend {
    // ★ 唯一的实质实现:任何未知设备字体都回退到系统 CJK 字体。
    fn load_device_font(&self, query: &FontQuery, register: &mut dyn FnMut(FontDefinition)) {
        if self.cjk_font.is_empty() {
            return;
        }
        register(FontDefinition::FontFile {
            name: query.name.clone(),
            is_bold: query.is_bold,
            is_italic: query.is_italic,
            // Arc<Vec<u8>> 强转 Arc<dyn AsRef<[u8]>>,共享同一份字节。
            data: FontFileData::new_shared(self.cjk_font.clone()),
            index: 0,
        });
    }

    // 以下全部照抄 NullUiBackend 的 no-op。
    fn mouse_visible(&self) -> bool {
        true
    }

    fn set_mouse_visible(&mut self, _visible: bool) {}

    fn set_mouse_cursor(&mut self, _cursor: MouseCursor) {}

    fn clipboard_content(&mut self) -> String {
        "".into()
    }

    fn set_clipboard_content(&mut self, _content: String) {}

    fn set_fullscreen(&mut self, _is_full: bool) -> Result<(), FullscreenError> {
        Ok(())
    }

    fn display_root_movie_download_failed_message(&self, _invalid_swf: bool, _fetch_error: String) {}

    fn message(&self, _message: &str) {}

    fn display_unsupported_video(&self, _url: Url) {}

    fn sort_device_fonts(
        &self,
        _query: &FontQuery,
        _register: &mut dyn FnMut(FontDefinition),
    ) -> Vec<FontQuery> {
        Vec::new()
    }

    fn open_virtual_keyboard(&self) {}

    fn close_virtual_keyboard(&self) {}

    fn language(&self) -> LanguageIdentifier {
        US_ENGLISH.clone()
    }

    fn display_file_open_dialog(&mut self, _filters: Vec<FileFilter>) -> Option<DialogResultFuture> {
        Some(Box::pin(async move { Ok(FileDialogResult::Canceled) }))
    }

    fn display_file_open_dialog_multiple(
        &mut self,
        _filters: Vec<FileFilter>,
    ) -> Option<MultiDialogResultFuture> {
        Some(Box::pin(async move { Ok(MultiFileDialogResult::Canceled) }))
    }

    fn close_file_dialog(&mut self) {}

    fn display_file_save_dialog(
        &mut self,
        _file_name: String,
        _domain: String,
    ) -> Option<DialogResultFuture> {
        None
    }
}
