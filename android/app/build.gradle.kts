plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("org.jetbrains.kotlin.plugin.compose")
}

// The desktop release's version, which android/build.sh passes in
// (-PthinktermVersion=0.5.0). The app speaks the mux codec of the release it
// was built from and needs a server on that same codec, so a phone build
// carries the number of the desktop and server it matches. A build without
// it is a local one and calls itself "dev".
val thinktermVersion = providers.gradleProperty("thinktermVersion").orNull ?: "dev"

// 0.5.0 -> 50000, 1.2.3 -> 1020300: each release above the one before, with
// the last two digits free for a phone-only fix of the same release
// (0.5.0.1). Anything else, a timestamped test build or "dev", gets 1, which
// every release is above.
fun versionCodeOf(version: String): Int {
    val parts = version.split('.').map { it.toIntOrNull() ?: return 1 }
    if (parts.size !in 3..4 || parts.drop(1).any { it > 99 }) return 1
    return parts[0] * 1_000_000 + parts[1] * 10_000 + parts[2] * 100 + parts.getOrElse(3) { 0 }
}

android {
    namespace = "com.roversx.thinkterm"
    compileSdk = 36

    defaultConfig {
        applicationId = "com.roversx.thinkterm"
        // Android 13: the glass (RuntimeShader) needs it.
        minSdk = 33
        targetSdk = 36
        versionCode = versionCodeOf(thinktermVersion)
        versionName = thinktermVersion
        // Phones are arm64. An emulator on a PC, with no ARM translation,
        // takes x86_64 too: -PthinktermAbis=arm64-v8a,x86_64, with the Rust
        // library built for it beside the arm64 one.
        ndk {
            abiFilters += providers.gradleProperty("thinktermAbis").orNull
                ?.split(',')?.map { it.trim() }?.filter { it.isNotEmpty() }
                ?.takeIf { it.isNotEmpty() } ?: listOf("arm64-v8a")
        }
    }

    buildTypes {
        release {
            // Most of Compose's icon set and of the libraries goes unused.
            isMinifyEnabled = true
            isShrinkResources = true
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
        }
    }
    // android/build.sh's Rust library. A release build makes its own copy
    // somewhere else and names it here, so a debug build running beside it
    // never packages the other's.
    providers.gradleProperty("thinktermJniLibs").orNull?.let { dir ->
        sourceSets.getByName("main").jniLibs.setSrcDirs(listOf(dir))
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    buildFeatures { compose = true }
    packaging {
        // JNA ships its own native pieces per ABI; only arm64 is built.
        jniLibs { useLegacyPackaging = false }
    }
}

kotlin {
    compilerOptions { jvmTarget.set(org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_17) }
}

dependencies {
    implementation("net.java.dev.jna:jna:5.17.0@aar")
    implementation(platform("androidx.compose:compose-bom:2026.03.00"))
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.material3:material3")
    implementation("androidx.compose.material:material-icons-extended")
    implementation("androidx.compose.ui:ui-tooling-preview")
    implementation("androidx.activity:activity-compose:1.11.0")
    implementation("androidx.lifecycle:lifecycle-runtime-compose:2.9.4")
    implementation("androidx.navigation:navigation-compose:2.9.5")
    implementation("androidx.security:security-crypto:1.1.0")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.10.2")
    // Liquid glass over the terminal (Apache-2.0). 1.0.6 is the last
    // release on Compose 1.10; 2.x needs Compose 1.12, AGP 9 and SDK 37.
    implementation("io.github.kyant0:backdrop:1.0.6")
    // The tailnet's login in the app, as iOS's Safari sheet has it.
    implementation("androidx.browser:browser:1.8.0")
}
