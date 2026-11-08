// The spike's Java side, bundled by dx as a Gradle module (manganis ffi).
plugins {
    id("com.android.library")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "com.alisonjenkins.pharos.tvnative.video"
    compileSdk = 34
    defaultConfig {
        minSdk = 28
    }
    kotlinOptions {
        jvmTarget = "17"
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
}
