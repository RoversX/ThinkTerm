// The app lock: with it on, the app opens behind a cover the phone's own
// check lifts (a face or a fingerprint, or the screen lock in their place).
// A module of its own, so the app reaches it only through AppLock and
// AppLockGate, and it knows nothing of the app: the words it shows are
// handed to it.
plugins {
    id("com.android.library")
    id("org.jetbrains.kotlin.android")
    id("org.jetbrains.kotlin.plugin.compose")
}

android {
    namespace = "com.roversx.thinkterm.applock"
    compileSdk = 36

    defaultConfig {
        // The app's: Android 13.
        minSdk = 33
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    buildFeatures { compose = true }
}

kotlin {
    compilerOptions { jvmTarget.set(org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_17) }
}

dependencies {
    // The app's own versions, so the two share one copy of each.
    implementation(platform("androidx.compose:compose-bom:2026.03.00"))
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.material3:material3")
    implementation("androidx.activity:activity-compose:1.11.0")
    implementation("androidx.lifecycle:lifecycle-runtime-compose:2.9.4")
}
