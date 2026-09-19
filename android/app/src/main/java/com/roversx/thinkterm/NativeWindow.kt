package com.roversx.thinkterm

/// The two JNI entries in libthinkterm_mobile.so that turn a Surface into
/// the ANativeWindow pointer the core's GPU side attaches to. The core
/// itself is reached through JNA (the generated bindings); these two need
/// the JNIEnv, so they are plain JNI and the library is loaded here.
object NativeWindow {
    init {
        System.loadLibrary("thinkterm_mobile")
    }

    @JvmStatic
    external fun fromSurface(surface: android.view.Surface): Long

    @JvmStatic
    external fun release(window: Long)
}
