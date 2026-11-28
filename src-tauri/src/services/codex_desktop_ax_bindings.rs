//! macOS Accessibility 的最小 C 绑定：只声明本模块真正用到的符号。
//!
//! # 为什么不用 crate，也不用 `core-foundation`
//!
//! 本仓库离线构建：`accessibility` / `accessibility-sys` 不在 cargo 缓存里。
//! `core-foundation` 只在依赖树里作为**传递**依赖，未声明为直接依赖——直接 `use` 它
//! 会在离线构建时因为版本选择与 feature 差异带来不确定性。
//!
//! `ApplicationServices` / `CoreFoundation` 都是系统框架，`#[link]` 即可用。
//! CFString 的构造与读取用最小手写 C 调用完成，够本模块使用。
//!
//! # 安全性
//!
//! 本模块的函数操作 Core Foundation 裸指针。约定：**所有接收 `AXUIElement` 的
//! pub 方法都是 `unsafe`**，调用方必须保证传入句柄来自同一个存活的
//! [`AxApplication`]，且不跨线程共享（AX 句柄非 `Send`/`Sync`）。
//!
//! # 只读 vs 写入
//!
//! 本模块只提供**原语**（取属性、取子项、设值、执行动作）。哪些原语可以调用，
//! 由上游 `codex_desktop_ax_macos` 按状态机约束决定——本模块不做策略判断。

#![cfg(target_os = "macos")]

use std::ffi::{c_void, CStr};
use std::os::raw::c_char;

pub type CFTypeRef = *const c_void;
pub type CFStringRef = *const c_void;
pub type CFArrayRef = *const c_void;
pub type CFAllocatorRef = *const c_void;
pub type CFIndex = isize;
pub type Boolean = u8;

/// AX 元素句柄。与 crate 内其它 AX 声明保持一致，统一用 `*const c_void`，
/// 避免同一符号在不同模块里以不同类型重复声明（rustc 会报警）。
pub type AXUIElement = *const c_void;

pub type AXError = i32;
pub const AX_SUCCESS: AXError = 0;
/// `kAXErrorNoValue` 与 `kAXErrorAttributeUnsupported` 在本平台同值。
pub const AX_NO_VALUE: AXError = -25205;
pub const AX_ATTRIBUTE_UNSUPPORTED: AXError = -25205;

/// `kCFStringEncodingUTF8`
const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
/// `kCFNumberSInt64Type`
const CF_NUMBER_SINT64: i32 = 4;

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXIsProcessTrustedWithOptions(options: CFTypeRef) -> bool;
    fn AXUIElementCreateApplication(pid: i32) -> AXUIElement;
    fn AXUIElementCopyAttributeValue(
        element: AXUIElement,
        attribute: CFStringRef,
        value: *mut CFTypeRef,
    ) -> AXError;
    fn AXUIElementSetAttributeValue(
        element: AXUIElement,
        attribute: CFStringRef,
        value: CFTypeRef,
    ) -> AXError;
    fn AXUIElementPerformAction(element: AXUIElement, action: CFStringRef) -> AXError;
    fn AXUIElementCopyActionNames(element: AXUIElement, names: *mut CFTypeRef) -> AXError;
    fn AXUIElementSetMessagingTimeout(element: AXUIElement, timeout_seconds: f32) -> AXError;
    fn AXUIElementGetTypeID() -> usize;
    fn AXUIElementIsAttributeSettable(
        element: AXUIElement,
        attribute: CFStringRef,
        settable: *mut Boolean,
    ) -> AXError;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(cf: CFTypeRef);
    fn CFRetain(cf: CFTypeRef) -> CFTypeRef;
    fn CFStringCreateWithCString(
        alloc: CFAllocatorRef,
        c_str: *const c_char,
        encoding: u32,
    ) -> CFStringRef;
    fn CFStringGetCString(
        the_string: CFStringRef,
        buffer: *mut c_char,
        buffer_size: CFIndex,
        encoding: u32,
    ) -> Boolean;
    fn CFStringGetLength(the_string: CFStringRef) -> CFIndex;
    fn CFStringGetMaximumSizeForEncoding(length: CFIndex, encoding: u32) -> CFIndex;
    fn CFArrayGetCount(the_array: CFArrayRef) -> CFIndex;
    fn CFArrayGetValueAtIndex(the_array: CFArrayRef, idx: CFIndex) -> *const c_void;
    fn CFNumberGetValue(number: CFTypeRef, the_type: i32, value_ptr: *mut c_void) -> Boolean;
    fn CFBooleanGetValue(boolean: CFTypeRef) -> Boolean;
    fn CFGetTypeID(cf: CFTypeRef) -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFNumberGetTypeID() -> usize;
    fn CFBooleanGetTypeID() -> usize;
    fn CFArrayGetTypeID() -> usize;
    fn CFDictionaryCreate(
        allocator: CFAllocatorRef,
        keys: *const CFTypeRef,
        values: *const CFTypeRef,
        num_values: CFIndex,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> CFTypeRef;
    /// CoreFoundation 把它们导出为 **`const CFBooleanRef` 变量**（指针变量），
    /// 不是值本身。Rust 里必须声明成 `*const c_void` 的 static，使用时解引用取到
    /// 真正的 `CFBooleanRef`。写成 `CFTypeRef` 会把"变量地址"当成值——
    /// 碰巧能跑，但语义错误且不可依赖。
    static kCFBooleanTrue: *const c_void;
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
    fn CFEqual(a: CFTypeRef, b: CFTypeRef) -> Boolean;
    fn CFHash(cf: CFTypeRef) -> usize;
}

/// 一组**被 retain 的** AX 元素，Drop 时逐个释放。
///
/// # 为什么需要它
///
/// `AXUIElementCopyAttributeValue(..., "AXChildren")` 返回的是一个 CFArray，
/// 里面的元素引用**归该数组所有**。函数返回后若释放数组，那些句柄就成了悬垂指针。
/// 因此本模块对每个元素 `CFRetain`，并交给这个拥有型容器管理——否则每次读子项
/// 都会泄漏（一轮发送会读几十次子项树）。
pub struct AxElements(Vec<AXUIElement>);

impl AxElements {
    /// 空集合。
    pub fn new() -> Self {
        Self(Vec::new())
    }

    /// 加入一个**已 retain** 的元素（所有权移交本容器，Drop 时统一释放）。
    ///
    /// 调用方必须保证传入的是自己拥有的引用：传裸借来的指针会 double-free。
    pub fn push(&mut self, retained: AXUIElement) {
        self.0.push(retained);
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn get(&self, index: usize) -> Option<AXUIElement> {
        self.0.get(index).copied()
    }
    pub fn first(&self) -> Option<AXUIElement> {
        self.0.first().copied()
    }
    pub fn iter(&self) -> std::slice::Iter<'_, AXUIElement> {
        self.0.iter()
    }
    /// 取出第一个元素并**移交所有权**给调用方（调用方负责 `CFRelease`）。
    pub fn into_first(mut self) -> Option<AXUIElement> {
        if self.0.is_empty() {
            self.0.clear();
            return None;
        }
        let first = self.0.swap_remove(0);
        // 其余元素由 Drop 释放；第一个的所有权转出，不再重复释放。
        Some(first)
    }
    /// 交出全部元素的所有权（调用方负责释放），用于需要跨函数传递的场合。
    pub fn into_vec(mut self) -> Vec<AXUIElement> {
        std::mem::take(&mut self.0)
    }
}

impl Default for AxElements {
    fn default() -> Self {
        Self::new()
    }
}

/// 借用迭代：`for child in &kids` 可用，且 `kids` 在整个循环期间保持存活。
///
/// **不实现** `IntoIterator for AxElements`（按值）：元素是裸指针（`Copy`），
/// 按值迭代会让"谁负责释放"变得含糊。需要移交所有权时用
/// [`AxElements::into_vec`] / [`AxElements::into_first`] 并显式管理。
impl<'a> IntoIterator for &'a AxElements {
    type Item = &'a AXUIElement;
    type IntoIter = std::slice::Iter<'a, AXUIElement>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl Drop for AxElements {
    fn drop(&mut self) {
        for e in self.0.drain(..) {
            if !e.is_null() {
                unsafe { CFRelease(e as CFTypeRef) };
            }
        }
    }
}

impl std::fmt::Debug for AxElements {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AxElements({})", self.0.len())
    }
}

/// 对单个元素 `CFRetain`，把它的生命周期从"父数组"延长到"调用方负责释放"。
///
/// # Safety
///
/// `element` 必须是一个**当前有效**的 AX 元素句柄（通常来自 `AxElements` 的子项，
/// 或 `attribute_element`）。返回的句柄由调用方负责 `CFRelease`。
pub unsafe fn retain_element(element: AXUIElement) -> AXUIElement {
    if !element.is_null() {
        CFRetain(element as CFTypeRef);
    }
    element
}

/// 释放一个由 [`retain_element`] 或 [`AxElements::into_vec`] 移交的元素。
///
/// # Safety
///
/// `element` 必须是从本模块取得所有权、且**尚未释放**的句柄。
pub unsafe fn release_element(element: AXUIElement) {
    if !element.is_null() {
        CFRelease(element as CFTypeRef);
    }
}

/// 单个 retained 元素的 RAII 守卫：`Drop` 时释放。
///
/// 用于"[retain_element] 拿到所有权 → 用一次 → 自动释放"这种短生命周期场合，
/// 避免每一处调用都要手写释放点（漏一处就是泄漏）。
pub struct AxElementGuard(AXUIElement);

impl AxElementGuard {
    /// 接管一个**已拥有所有权**的句柄。
    ///
    /// # Safety
    ///
    /// `element` 必须是从本模块取得的、尚未释放的句柄。
    pub unsafe fn from_owned(element: AXUIElement) -> Self {
        Self(element)
    }

    pub fn get(&self) -> AXUIElement {
        self.0
    }
}

impl Drop for AxElementGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0 as CFTypeRef) };
        }
    }
}

/// 进程是否已被授予 Accessibility 权限。
pub fn is_process_trusted() -> bool {
    unsafe { AXIsProcessTrusted() }
}

/// 一个 CFString 的 RAII 包装（仅用于"我们创建、我们释放"的场合）。
struct OwnedCfString(CFStringRef);

impl OwnedCfString {
    fn new(s: &str) -> Option<Self> {
        let c = std::ffi::CString::new(s).ok()?;
        let cf = unsafe {
            CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), CF_STRING_ENCODING_UTF8)
        };
        if cf.is_null() {
            None
        } else {
            Some(Self(cf))
        }
    }
    fn as_ref(&self) -> CFStringRef {
        self.0
    }
}

impl Drop for OwnedCfString {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) };
        }
    }
}

/// 把一个 CFString 转成 Rust `String`。**不**释放入参。
fn cfstring_to_string(s: CFStringRef) -> Option<String> {
    if s.is_null() {
        return None;
    }
    unsafe {
        if CFGetTypeID(s) != CFStringGetTypeID() {
            return None;
        }
        let len = CFStringGetLength(s);
        let max = CFStringGetMaximumSizeForEncoding(len, CF_STRING_ENCODING_UTF8) + 1;
        if max <= 0 {
            return None;
        }
        let mut buf = vec![0 as c_char; max as usize];
        let ok = CFStringGetCString(s, buf.as_mut_ptr(), max, CF_STRING_ENCODING_UTF8);
        if ok == 0 {
            return None;
        }
        CStr::from_ptr(buf.as_ptr())
            .to_str()
            .ok()
            .map(|v| v.to_string())
    }
}

/// 持有到某个应用根元素的句柄，负责释放。
pub struct AxApplication {
    element: AXUIElement,
}

impl AxApplication {
    pub fn new(pid: u32) -> Option<Self> {
        let element = unsafe { AXUIElementCreateApplication(pid as i32) };
        if element.is_null() {
            return None;
        }
        let app = Self { element };
        // 单次查询的等待上限：超时比"卡死"更安全（上游按读失败处理）。
        unsafe {
            AXUIElementSetMessagingTimeout(app.element, 2.0f32);
        }
        Some(app)
    }

    /// 取根元素句柄。**裸指针**：调用方必须保证在 `self` 存活期间使用。
    pub fn as_element(&self) -> AXUIElement {
        self.element
    }

    /// 读取字符串属性。
    ///
    /// # Safety
    ///
    /// `element` 必须是本 `AxApplication` 子树里一个仍存活的句柄
    /// （不能跨 `AxApplication` 使用，也不能在父引用被释放后使用）。
    pub unsafe fn attribute_string(&self, element: AXUIElement, name: &str) -> Option<String> {
        let cf_name = OwnedCfString::new(name)?;
        let mut value: CFTypeRef = std::ptr::null();
        let err = unsafe { AXUIElementCopyAttributeValue(element, cf_name.as_ref(), &mut value) };
        if err != AX_SUCCESS || value.is_null() {
            return None;
        }
        let out = cfstring_to_string(value as CFStringRef);
        unsafe { CFRelease(value) };
        out
    }

    /// 读取整数属性（`AXNumberOfCharacters` 等）。
    ///
    /// # Safety
    ///
    /// `element` 必须是本 `AxApplication` 子树里一个仍存活的句柄
    /// （不能跨 `AxApplication` 使用，也不能在父引用被释放后使用）。
    pub unsafe fn attribute_int(&self, element: AXUIElement, name: &str) -> Option<i64> {
        let cf_name = OwnedCfString::new(name)?;
        let mut value: CFTypeRef = std::ptr::null();
        let err = unsafe { AXUIElementCopyAttributeValue(element, cf_name.as_ref(), &mut value) };
        if err != AX_SUCCESS || value.is_null() {
            return None;
        }
        let mut out: i64 = 0;
        let ok = unsafe {
            CFGetTypeID(value) == CFNumberGetTypeID()
                && CFNumberGetValue(value, CF_NUMBER_SINT64, &mut out as *mut i64 as *mut c_void)
                    != 0
        };
        unsafe { CFRelease(value) };
        if ok {
            Some(out)
        } else {
            None
        }
    }

    /// 读取布尔属性（`AXEnabled` 等）。
    ///
    /// # Safety
    ///
    /// `element` 必须是本 `AxApplication` 子树里一个仍存活的句柄
    /// （不能跨 `AxApplication` 使用，也不能在父引用被释放后使用）。
    pub unsafe fn attribute_bool(&self, element: AXUIElement, name: &str) -> Option<bool> {
        let cf_name = OwnedCfString::new(name)?;
        let mut value: CFTypeRef = std::ptr::null();
        let err = unsafe { AXUIElementCopyAttributeValue(element, cf_name.as_ref(), &mut value) };
        if err != AX_SUCCESS || value.is_null() {
            return None;
        }
        let out = unsafe {
            if CFGetTypeID(value) == CFBooleanGetTypeID() {
                Some(CFBooleanGetValue(value) != 0)
            } else {
                None
            }
        };
        unsafe { CFRelease(value) };
        out
    }

    /// 读取字符串数组属性（`AXDOMClassList`）。
    ///
    /// # Safety
    ///
    /// `element` 必须是本 `AxApplication` 子树里一个仍存活的句柄
    /// （不能跨 `AxApplication` 使用，也不能在父引用被释放后使用）。
    pub unsafe fn attribute_string_array(
        &self,
        element: AXUIElement,
        name: &str,
    ) -> Option<Vec<String>> {
        let cf_name = OwnedCfString::new(name)?;
        let mut value: CFTypeRef = std::ptr::null();
        let err = unsafe { AXUIElementCopyAttributeValue(element, cf_name.as_ref(), &mut value) };
        if err != AX_SUCCESS || value.is_null() {
            return None;
        }
        let out = unsafe {
            if CFGetTypeID(value) != CFArrayGetTypeID() {
                None
            } else {
                let arr = value as CFArrayRef;
                let n = CFArrayGetCount(arr);
                let mut v = Vec::with_capacity(n.max(0) as usize);
                for i in 0..n {
                    let item = CFArrayGetValueAtIndex(arr, i);
                    if let Some(s) = cfstring_to_string(item as CFStringRef) {
                        v.push(s);
                    }
                }
                Some(v)
            }
        };
        unsafe { CFRelease(value) };
        out
    }

    /// 以 AX 元素数组读取属性（`AXChildren`）。
    ///
    /// 返回的元素不持有所有权；调用方须在父引用仍有效时使用。
    ///
    /// # Safety
    ///
    /// `element` 必须是本 `AxApplication` 子树里一个仍存活的句柄
    /// （不能跨 `AxApplication` 使用，也不能在父引用被释放后使用）。
    pub unsafe fn attribute_elements(
        &self,
        element: AXUIElement,
        name: &str,
    ) -> Option<AxElements> {
        let cf_name = OwnedCfString::new(name)?;
        let mut value: CFTypeRef = std::ptr::null();
        let err = unsafe { AXUIElementCopyAttributeValue(element, cf_name.as_ref(), &mut value) };
        if err != AX_SUCCESS || value.is_null() {
            return None;
        }
        let out = unsafe {
            if CFGetTypeID(value) != CFArrayGetTypeID() {
                None
            } else {
                let arr = value as CFArrayRef;
                let n = CFArrayGetCount(arr);
                let mut v = Vec::with_capacity(n.max(0) as usize);
                for i in 0..n {
                    let item = CFArrayGetValueAtIndex(arr, i) as AXUIElement;
                    if !item.is_null() {
                        // 元素引用归数组所有：必须**逐个 retain**，否则函数返回后
                        // 数组一旦释放，这些句柄就是悬垂指针。
                        CFRetain(item as CFTypeRef);
                        v.push(item);
                    }
                }
                Some(AxElements(v))
            }
        };
        // 现在可以安全释放数组：里面的元素已被逐个 retain，由 `AxElements` 接管。
        unsafe { CFRelease(value) };
        out
    }

    /// 读取**单个** AX 元素属性（`AXParent` 等——它们不是数组）。
    ///
    /// 与 [`Self::attribute_elements`] 的区别：后者用于 `AXChildren`/`AXWindows`
    /// 这类数组属性。把 `AXParent` 当数组读会得到 `None`，从而让"向上回溯"
    /// 在第一步就断掉（这个 bug 曾在真机上导致侧栏定位失败）。
    ///
    /// # Safety
    ///
    /// `element` 必须是本 `AxApplication` 子树里一个仍存活的句柄。
    pub unsafe fn attribute_element(
        &self,
        element: AXUIElement,
        name: &str,
    ) -> Option<AXUIElement> {
        let cf_name = OwnedCfString::new(name)?;
        let mut value: CFTypeRef = std::ptr::null();
        let err = unsafe { AXUIElementCopyAttributeValue(element, cf_name.as_ref(), &mut value) };
        if err != AX_SUCCESS || value.is_null() {
            return None;
        }
        // 该属性必须是 AXUIElement 类型；其它类型一律当作不可用。
        if unsafe { CFGetTypeID(value) } != unsafe { AXUIElementGetTypeID() } {
            unsafe { CFRelease(value) };
            return None;
        }
        // 转移所有权给调用方：调用方负责后续使用；本函数不释放。
        Some(value as AXUIElement)
    }

    /// 该属性是否可写。
    ///
    /// # Safety
    ///
    /// `element` 必须是本 `AxApplication` 子树里一个仍存活的句柄
    /// （不能跨 `AxApplication` 使用，也不能在父引用被释放后使用）。
    pub unsafe fn is_settable(&self, element: AXUIElement, name: &str) -> bool {
        let Some(cf_name) = OwnedCfString::new(name) else {
            return false;
        };
        let mut settable: Boolean = 0;
        let err =
            unsafe { AXUIElementIsAttributeSettable(element, cf_name.as_ref(), &mut settable) };
        err == AX_SUCCESS && settable != 0
    }

    /// 写入字符串属性（`AXValue`）。返回是否成功。
    ///
    /// **副作用原语。** 调用前必须已通过空稿核证与目标核证。
    ///
    /// # Safety
    ///
    /// `element` 必须是本 `AxApplication` 子树里一个仍存活的句柄
    /// （不能跨 `AxApplication` 使用，也不能在父引用被释放后使用）。
    pub unsafe fn set_string(&self, element: AXUIElement, name: &str, value: &str) -> bool {
        let (Some(cf_name), Some(cf_value)) = (OwnedCfString::new(name), OwnedCfString::new(value))
        else {
            return false;
        };
        let err =
            unsafe { AXUIElementSetAttributeValue(element, cf_name.as_ref(), cf_value.as_ref()) };
        err == AX_SUCCESS
    }

    /// 执行动作（`AXPress`）。返回是否成功。
    ///
    /// **副作用原语，且效果未知**：返回 `false` 也可能已生效。
    ///
    /// # Safety
    ///
    /// `element` 必须是本 `AxApplication` 子树里一个仍存活的句柄
    /// （不能跨 `AxApplication` 使用，也不能在父引用被释放后使用）。
    pub unsafe fn perform_action(&self, element: AXUIElement, action: &str) -> bool {
        let Some(cf_action) = OwnedCfString::new(action) else {
            return false;
        };
        let err = unsafe { AXUIElementPerformAction(element, cf_action.as_ref()) };
        err == AX_SUCCESS
    }

    /// 该元素支持的动作名列表。
    ///
    /// # Safety
    ///
    /// `element` 必须是本 `AxApplication` 子树里一个仍存活的句柄
    /// （不能跨 `AxApplication` 使用，也不能在父引用被释放后使用）。
    pub unsafe fn action_names(&self, element: AXUIElement) -> Vec<String> {
        let mut names: CFTypeRef = std::ptr::null();
        let err = unsafe { AXUIElementCopyActionNames(element, &mut names) };
        if err != AX_SUCCESS || names.is_null() {
            return Vec::new();
        }
        let out = unsafe {
            if CFGetTypeID(names) != CFArrayGetTypeID() {
                Vec::new()
            } else {
                let arr = names as CFArrayRef;
                let n = CFArrayGetCount(arr);
                (0..n)
                    .filter_map(|i| {
                        cfstring_to_string(CFArrayGetValueAtIndex(arr, i) as CFStringRef)
                    })
                    .collect()
            }
        };
        unsafe { CFRelease(names) };
        out
    }

    /// 角色的便捷读取。
    ///
    /// # Safety
    ///
    /// `element` 必须是本 `AxApplication` 子树里一个仍存活的句柄。
    pub unsafe fn role(&self, element: AXUIElement) -> Option<String> {
        self.attribute_string(element, "AXRole")
    }

    /// 直接子项。
    ///
    /// # Safety
    ///
    /// `element` 必须是本 `AxApplication` 子树里一个仍存活的句柄。
    pub unsafe fn children(&self, element: AXUIElement) -> AxElements {
        self.attribute_elements(element, "AXChildren")
            .unwrap_or_else(|| AxElements(Vec::new()))
    }
}

impl Drop for AxApplication {
    fn drop(&mut self) {
        if !self.element.is_null() {
            unsafe { CFRelease(self.element as CFTypeRef) };
        }
    }
}

/// 两个 AX 元素是否同一（AX 可能为同一元素返回不同包装对象）。
///
/// # Safety
///
/// 两个句柄都必须来自同一个存活 `AxApplication`。
pub unsafe fn cf_equal(a: AXUIElement, b: AXUIElement) -> bool {
    CFEqual(a, b) != 0
}

/// 元素的 `CFHash`，仅用于诊断与去重（不参与判定）。
///
/// # Safety
///
/// `element` 必须来自一个存活 `AxApplication`。
pub unsafe fn cf_hash_of(element: AXUIElement) -> usize {
    CFHash(element)
}

/// 请求辅助功能权限：**带系统弹窗**的那种。
///
/// 本机 macOS 的「隐私与安全性」面板不列「辅助功能」这一项，无法手动添加裸二进制；
/// 只有 App 自己调用 `AXIsProcessTrustedWithOptions` 并带上
/// `kAXTrustedCheckOptionPrompt = true`，系统才会弹授权对话框并把本进程加入 TCC。
///
/// 返回当前是否已被信任（弹窗是异步的：首次调用通常返回 `false`，
/// 用户在对话框里点「打开系统设置」并授权后，下次调用才为 `true`）。
pub fn request_trust_with_prompt() -> bool {
    unsafe {
        // kAXTrustedCheckOptionPrompt
        let Some(key) = OwnedCfString::new("AXTrustedCheckOptionPrompt") else {
            return is_process_trusted();
        };
        let keys: [CFTypeRef; 1] = [key.as_ref() as CFTypeRef];
        // `kCFBooleanTrue` 是 CoreFoundation 导出的指针变量；读取它就是取其值
        // （immutable singleton，无需 retain/release）。外层已是 unsafe 块。
        let true_ref: CFTypeRef = kCFBooleanTrue;
        let values: [CFTypeRef; 1] = [true_ref];
        let dict = CFDictionaryCreate(
            std::ptr::null(),
            keys.as_ptr(),
            values.as_ptr(),
            1,
            // 两者声明为 `c_void`，取地址即 `*const c_void`，无需再转。
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        );
        if dict.is_null() {
            return is_process_trusted();
        }
        let trusted = AXIsProcessTrustedWithOptions(dict);
        CFRelease(dict);
        trusted
    }
}
