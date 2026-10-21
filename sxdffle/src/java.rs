use jni::objects::{
    JByteArray, JClass, JIntArray, JMethodID, JObject, JObjectArray, JString, JValue, ReleaseMode,
};
use jni::signature::{Primitive, ReturnType};
use jni::{jni_sig, jni_str, Env};
use ruffle_core::ContextMenuItem;
use std::path::PathBuf;
use std::sync::OnceLock;

/// Handles to various items on the Java `PlayerActivity` class.
/// This is statically initialized once at startup, via the Java method `nativeInit()`.
/// This avoids needing to pay the lookup and validation penalty for every single call back into Java,
/// which can be a significant cost.
pub struct JavaInterface {
    get_surface_width: JMethodID,
    get_surface_height: JMethodID,
    show_context_menu: JMethodID,
    get_swf_bytes: JMethodID,
    get_swf_uri: JMethodID,
    get_trace_output: JMethodID,
    get_loc_in_window: JMethodID,
    get_android_data_storage_dir: JMethodID,
    // ---- SXDffle 定制：把浏览器侧的登录态与防盗链信息交给 Flash 引擎 ----
    get_referer: JMethodID,
    get_cookie: JMethodID,
    /// SXDffle：页面上的 flashVars（LoaderInfo.parameters 的来源）
    get_flash_vars: JMethodID,
}

static JAVA_INTERFACE: OnceLock<JavaInterface> = OnceLock::new();

impl JavaInterface {
    pub fn get_surface_width(env: &mut Env, this: &JObject) -> i32 {
        let result = unsafe {
            env.call_method_unchecked(
                this,
                Self::get().get_surface_width,
                ReturnType::Primitive(Primitive::Int),
                &[],
            )
        };
        result
            .expect("getSurfaceWidth() must never throw")
            .i()
            .unwrap()
    }

    pub fn get_surface_height(env: &mut Env, this: &JObject) -> i32 {
        let result = unsafe {
            env.call_method_unchecked(
                this,
                Self::get().get_surface_height,
                ReturnType::Primitive(Primitive::Int),
                &[],
            )
        };
        result
            .expect("getSurfaceHeight() must never throw")
            .i()
            .unwrap()
    }

    pub fn show_context_menu(env: &mut Env, this: &JObject, items: &[ContextMenuItem]) {
        let arr = JObjectArray::<JString>::new(env, items.len(), JString::null()).unwrap();
        for (i, e) in items.iter().enumerate() {
            let s = env
                .new_string(format!(
                    "{} {} {} {}",
                    e.enabled, e.separator_before, e.checked, e.caption
                ))
                .unwrap();
            arr.set_element(env, i, &s).unwrap();
        }
        let result = unsafe {
            env.call_method_unchecked(
                this,
                Self::get().show_context_menu,
                ReturnType::Primitive(Primitive::Void),
                &[JValue::Object(arr.as_ref()).as_jni()],
            )
        };
        result.expect("showContextMenu() must never throw");
    }

    pub fn get_swf_bytes(env: &mut Env, this: &JObject) -> Option<Vec<u8>> {
        let result = unsafe {
            env.call_method_unchecked(this, Self::get().get_swf_bytes, ReturnType::Array, &[])
        };
        let object = result.expect("getSwfBytes() must never throw").l().unwrap();
        if object.is_null() {
            return None;
        }

        let arr = unsafe { JByteArray::from_raw(env, object.into_raw()) };
        let elements = unsafe { arr.get_elements(env, ReleaseMode::NoCopyBack).unwrap() };
        let data =
            unsafe { std::slice::from_raw_parts(elements.as_ptr() as *mut u8, elements.len()) };
        Some(data.to_vec())
    }

    pub fn get_swf_uri(env: &mut Env, this: &JObject) -> String {
        let result = unsafe {
            env.call_method_unchecked(this, Self::get().get_swf_uri, ReturnType::Object, &[])
        };
        let object = result.expect("getSwfUri() must never throw").l().unwrap();
        if object.is_null() {
            return Default::default();
        }
        let string_object = unsafe { env.as_cast_unchecked::<JString>(&object) };
        string_object.try_to_string(env).unwrap()
    }

    pub fn get_trace_output(env: &mut Env, this: &JObject) -> Option<PathBuf> {
        let result = unsafe {
            env.call_method_unchecked(this, Self::get().get_trace_output, ReturnType::Object, &[])
        };
        let object = result
            .expect("getTraceOutput() must never throw")
            .l()
            .unwrap();
        if object.is_null() {
            return None;
        }
        let string_object = unsafe { env.as_cast_unchecked::<JString>(&object) };
        let url = string_object.try_to_string(env).unwrap();
        Some(url.into())
    }

    pub fn get_loc_in_window(env: &mut Env, this: &JObject) -> (i32, i32) {
        let result = unsafe {
            env.call_method_unchecked(this, Self::get().get_loc_in_window, ReturnType::Array, &[])
        };
        let object = result
            .expect("getLocInWindow() must never throw")
            .l()
            .unwrap();
        let arr = unsafe { JIntArray::from_raw(env, object.into_raw()) };
        let elements = unsafe { arr.get_elements(env, ReleaseMode::NoCopyBack).unwrap() };
        let data = unsafe { std::slice::from_raw_parts(elements.as_ptr(), elements.len()) };
        (data[0], data[1])
    }

    pub fn get_android_data_storage_dir(env: &mut Env, this: &JObject) -> PathBuf {
        let result = unsafe {
            env.call_method_unchecked(
                this,
                Self::get().get_android_data_storage_dir,
                ReturnType::Object,
                &[],
            )
        };
        let object = result
            .expect("getAndroidDataStorageDir() must never throw")
            .l()
            .unwrap();
        let string_object = unsafe { env.as_cast_unchecked::<JString>(&object) };
        let path = string_object.try_to_string(env).unwrap();
        PathBuf::from(path)
    }

    /// SXDffle：取浏览器侧传入的 Referer（游戏页地址）。
    /// 页游服务器常用 Referer 做防盗链校验，缺失会导致资源 403。
    pub fn get_referer(env: &mut Env, this: &JObject) -> Option<String> {
        Self::get_opt_string(env, this, Self::get().get_referer, "getReferer")
    }

    /// SXDffle：取浏览器侧传入的 Cookie（WebView 登录后的会话）。
    /// 这是"网页登录 → 进入游戏"链路能打通的关键。
    pub fn get_cookie(env: &mut Env, this: &JObject) -> Option<String> {
        Self::get_opt_string(env, this, Self::get().get_cookie, "getCookie")
    }

    /// SXDffle：取页面上的 flashVars 原始串（形如 `k1=v1&k2=v2`，值可能是 URL 编码的）。
    ///
    /// 页游的启动参数全部走 flashVars（如《神仙道》的 ip/port/hash_code 等 170+ 项），
    /// 没有它游戏无法完成初始化。解码在 `lib.rs` 中用 `form_urlencoded::parse`
    /// 完成 —— 这与 Flash Player 对 flashvars 的处理语义一致。
    pub fn get_flash_vars(env: &mut Env, this: &JObject) -> Option<String> {
        Self::get_opt_string(env, this, Self::get().get_flash_vars, "getFlashVars")
    }

    fn get_opt_string(
        env: &mut Env,
        this: &JObject,
        method: JMethodID,
        name: &str,
    ) -> Option<String> {
        let result =
            unsafe { env.call_method_unchecked(this, method, ReturnType::Object, &[]) };
        let object = result
            .unwrap_or_else(|_| panic!("{}() must never throw", name))
            .l()
            .unwrap();
        if object.is_null() {
            return None;
        }
        let string_object = unsafe { env.as_cast_unchecked::<JString>(&object) };
        let value = string_object.try_to_string(env).unwrap();
        if value.is_empty() {
            None
        } else {
            Some(value)
        }
    }

    pub fn get() -> &'static JavaInterface {
        JAVA_INTERFACE
            .get()
            .expect("Java interface must have been created via nativeInit()")
    }

    pub fn init(env: &mut Env, class: &JClass) {
        let _ = JAVA_INTERFACE.set(JavaInterface {
            get_surface_width: env
                .get_method_id(class, jni_str!("getSurfaceWidth"), jni_sig!("()I"))
                .expect("getSurfaceWidth must exist"),
            get_surface_height: env
                .get_method_id(class, jni_str!("getSurfaceHeight"), jni_sig!("()I"))
                .expect("getSurfaceHeight must exist"),
            show_context_menu: env
                .get_method_id(
                    class,
                    jni_str!("showContextMenu"),
                    jni_sig!("([Ljava/lang/String;)V"),
                )
                .expect("showContextMenu must exist"),
            get_swf_bytes: env
                .get_method_id(class, jni_str!("getSwfBytes"), jni_sig!("()[B"))
                .expect("getSwfBytes must exist"),
            get_swf_uri: env
                .get_method_id(
                    class,
                    jni_str!("getSwfUri"),
                    jni_sig!("()Ljava/lang/String;"),
                )
                .expect("getSwfUri must exist"),
            get_trace_output: env
                .get_method_id(
                    class,
                    jni_str!("getTraceOutput"),
                    jni_sig!("()Ljava/lang/String;"),
                )
                .expect("getTraceOutput must exist"),
            get_loc_in_window: env
                .get_method_id(class, jni_str!("getLocInWindow"), jni_sig!("()[I"))
                .expect("getLocInWindow must exist"),
            get_android_data_storage_dir: env
                .get_method_id(
                    class,
                    jni_str!("getAndroidDataStorageDir"),
                    jni_sig!("()Ljava/lang/String;"),
                )
                .expect("getAndroidDataStorageDir must exist"),
            // 这两个是 SXDffle 新增的；用 get_method_id 的 Result 容忍缺失，
            // 以保证即便宿主 Activity 尚未实现也不会启动崩溃。
            get_referer: env
                .get_method_id(
                    class,
                    jni_str!("getReferer"),
                    jni_sig!("()Ljava/lang/String;"),
                )
                .expect("getReferer must exist"),
            get_cookie: env
                .get_method_id(class, jni_str!("getCookie"), jni_sig!("()Ljava/lang/String;"))
                .expect("getCookie must exist"),
            get_flash_vars: env
                .get_method_id(
                    class,
                    jni_str!("getFlashVars"),
                    jni_sig!("()Ljava/lang/String;"),
                )
                .expect("getFlashVars must exist"),
        });
    }
}
