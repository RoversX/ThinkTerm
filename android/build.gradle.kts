// The Android shell: Kotlin and Compose over the shared Rust core, which
// android/build.sh builds into app/src/main/jniLibs and binds through
// UniFFI's generated Kotlin.
plugins {
    id("com.android.application") version "8.13.0" apply false
    id("org.jetbrains.kotlin.android") version "2.2.20" apply false
    id("org.jetbrains.kotlin.plugin.compose") version "2.2.20" apply false
}
