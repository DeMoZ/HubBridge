plugins {
    id("com.android.application")
}

android {
    namespace = "com.demoz.hubbridge"
    compileSdk = 36
    ndkVersion = "27.2.12479018"

    defaultConfig {
        applicationId = "com.demoz.hubbridge"
        minSdk = 26
        targetSdk = 36
        versionCode = 1
        versionName = "0.1.0"
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
}

// Build the Rust agent core (crates/android) into src/main/jniLibs with cargo-ndk.
// Skip with -PskipCargo when the .so files are already there (e.g. built by CI).
val repoRoot = rootProject.projectDir.parentFile
val cargoNdk = tasks.register<Exec>("cargoNdk") {
    description = "Builds libhubbridge_android.so for all ABIs"
    workingDir = repoRoot
    val release = gradle.startParameter.taskNames.any { it.contains("Release", ignoreCase = true) }
    val args = mutableListOf(
        "cargo", "ndk",
        "-t", "arm64-v8a", "-t", "armeabi-v7a", "-t", "x86_64",
        "-P", "26",
        "-o", file("src/main/jniLibs").absolutePath,
        "build", "-p", "hubbridge-android",
    )
    if (release) args += "--release"
    commandLine(args)
    val ndkDir = androidComponents.sdkComponents.ndkDirectory
    doFirst {
        environment("ANDROID_NDK_HOME", ndkDir.get().asFile.absolutePath)
    }
}

if (!project.hasProperty("skipCargo")) {
    tasks.named("preBuild") { dependsOn(cargoNdk) }
}
