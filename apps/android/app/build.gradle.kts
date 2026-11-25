// Waypane: window-level Wayland forwarding over SSH.
// Copyright (C) 2026  Zhou Qiankang <wszqkzqk@qq.com>
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.
//
// SPDX-License-Identifier: GPL-3.0-or-later

import java.util.Properties

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.plugin.compose")
    id("org.jetbrains.kotlin.plugin.serialization")
}

val localSigningProperties = Properties().apply {
    val propertiesFile = rootDir.resolve("signing/keystore.properties")
    if (propertiesFile.isFile) {
        propertiesFile.inputStream().use { load(it) }
    }
}

fun signingSetting(environmentName: String, propertyName: String): String? =
    System.getenv(environmentName)?.takeIf { it.isNotBlank() }
        ?: localSigningProperties.getProperty(propertyName)?.takeIf { it.isNotBlank() }

val releaseKeystoreFile = signingSetting("WAYPANE_KEYSTORE_FILE", "storeFile")

fun gitOutput(vararg arguments: String): String? =
    try {
        // providers.exec: plain exec is rejected at configuration time by Gradle 9's configuration cache.
        providers.exec { commandLine("git", *arguments) }
            .standardOutput.asText.get()
            .trim()
            .ifBlank { null }
    } catch (_: Exception) {
        null
    }

// APK version from git: commit count grows for every build; vX.Y.Z tags name releases;
// --always keeps untagged history identifiable by commit hash. CI needs fetch-depth: 0.
val apkVersionCode = gitOutput("rev-list", "--count", "HEAD")?.toIntOrNull() ?: 1
val apkVersionName = gitOutput("describe", "--tags", "--abbrev=12", "--always")
    ?.removePrefix("v")
    ?.replace('-', '.')
    ?: "0.1.0"

val rustJniLibs = layout.buildDirectory.dir("rustJniLibs")
val rustCrateDir = rootDir.resolve("../../crates/waypane-android").normalize()

val rustBuild = tasks.register<Exec>("rustBuild") {
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
    compileSdk = 37
    buildToolsVersion = "37.0.0"

    defaultConfig {
        applicationId = "org.waypane.app"
        minSdk = 26
        targetSdk = 37
        ndk {
            abiFilters += listOf("arm64-v8a")
        }
        versionCode = apkVersionCode
        versionName = apkVersionName
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
    }

    buildFeatures {
        compose = true
        buildConfig = false
    }

    signingConfigs {
        create("release") {
            if (!releaseKeystoreFile.isNullOrBlank()) {
                storeFile = file(releaseKeystoreFile)
                storePassword = signingSetting("WAYPANE_KEYSTORE_PASSWORD", "storePassword")
                keyAlias = signingSetting("WAYPANE_KEY_ALIAS", "keyAlias")
                keyPassword = signingSetting("WAYPANE_KEY_PASSWORD", "keyPassword")
            }
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = true
            isShrinkResources = true
            signingConfig = if (releaseKeystoreFile.isNullOrBlank()) {
                signingConfigs.getByName("debug")
            } else {
                signingConfigs.getByName("release")
            }
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    packaging {
        resources.excludes += "/META-INF/{AL2.0,LGPL2.1}"
    }

    sourceSets.getByName("main") {
        jniLibs.directories.add(rustJniLibs.get().asFile.absolutePath)
    }
}

kotlin {
    compilerOptions {
        jvmTarget.set(org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_17)
    }
}

tasks.matching { it.name.startsWith("merge") && it.name.contains("JniLibFolders") }
    .configureEach { dependsOn(rustBuild) }

dependencies {
    implementation(platform("androidx.compose:compose-bom:2026.09.00"))
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.ui:ui-graphics")
    implementation("androidx.compose.ui:ui-tooling-preview")
    implementation("androidx.compose.material3:material3")
    implementation("androidx.core:core-ktx:1.19.1")
    implementation("androidx.activity:activity-compose:1.13.0")
    implementation("androidx.lifecycle:lifecycle-runtime-ktx:2.11.0")
    implementation("androidx.lifecycle:lifecycle-viewmodel-compose:2.11.0")
    implementation("org.jetbrains.kotlinx:kotlinx-serialization-json:1.13.0")
    debugImplementation("androidx.compose.ui:ui-tooling")
    debugImplementation("androidx.compose.ui:ui-test-manifest")
}
