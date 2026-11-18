plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("org.jetbrains.kotlin.plugin.serialization")
}

val rustJniLibs = layout.buildDirectory.dir("rustJniLibs")
val rustCrateDir = rootDir.resolve("../../crates/waypane-android").normalize()

val rustBuild by tasks.registering(Exec::class) {
    group = "build"
    description = "Builds the Waypane Rust JNI library for arm64 Android."
    workingDir = rootDir.resolve("../../").normalize()
    inputs.dir(rustCrateDir)
    inputs.file(rootDir.resolve("../../Cargo.lock"))
    outputs.dir(rustJniLibs)
    commandLine(
        "cargo",
        "ndk",
        "--target", "arm64-v8a",
        "--platform", "26",
        "--output-dir", rustJniLibs.get().asFile.absolutePath,
        "build",
        "--locked",
        "--release",
        "-p", "waypane-android",
        "--features", "jni-check",
    )
    doFirst {
        val ndk = System.getenv("ANDROID_NDK_HOME") ?: System.getenv("ANDROID_NDK_ROOT")
        require(!ndk.isNullOrBlank()) {
            "ANDROID_NDK_HOME is required. Install Android NDK and cargo-ndk before building."
        }
    }
}

android {
    namespace = "org.waypane.app"
    compileSdk = 35

    defaultConfig {
        applicationId = "org.waypane.app"
        minSdk = 26
        targetSdk = 35
        versionCode = 1
        versionName = "0.1.0"
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
    }

    buildFeatures {
        compose = true
        buildConfig = false
    }

    composeOptions {
        kotlinCompilerExtensionVersion = "1.5.15"
    }

    buildTypes {
        release {
            isMinifyEnabled = true
            isShrinkResources = true
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    kotlinOptions {
        jvmTarget = "17"
    }

    packaging {
        resources.excludes += "/META-INF/{AL2.0,LGPL2.1}"
    }

    sourceSets.getByName("main") {
        jniLibs.srcDir(rustJniLibs)
    }
}

tasks.matching { it.name.startsWith("merge") && it.name.contains("JniLibFolders") }
    .configureEach { dependsOn(rustBuild) }

dependencies {
    implementation(platform("androidx.compose:compose-bom:2024.12.01"))
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.ui:ui-graphics")
    implementation("androidx.compose.ui:ui-tooling-preview")
    implementation("androidx.compose.material3:material3")
    implementation("androidx.core:core-ktx:1.15.0")
    implementation("androidx.activity:activity-compose:1.9.3")
    implementation("androidx.lifecycle:lifecycle-runtime-ktx:2.8.7")
    implementation("androidx.lifecycle:lifecycle-viewmodel-compose:2.8.7")
    implementation("org.jetbrains.kotlinx:kotlinx-serialization-json:1.7.2")
    debugImplementation("androidx.compose.ui:ui-tooling")
    debugImplementation("androidx.compose.ui:ui-test-manifest")
}
