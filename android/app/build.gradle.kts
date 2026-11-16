plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("org.jetbrains.kotlin.plugin.compose")
}

android {
    namespace = "com.roversx.thinkterm"
    compileSdk = 36

    defaultConfig {
        applicationId = "com.roversx.thinkterm"
        minSdk = 35
        targetSdk = 36
        versionCode = 1
        versionName = "0.1"
        ndk { abiFilters += listOf("arm64-v8a") }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
        }
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
