plugins {
    id("com.android.application")
}

val workspaceVersion: String = run {
    val cargoToml = rootProject.projectDir.parentFile.resolve("Cargo.toml")
    val text = cargoToml.readText()
    val section = Regex("""(?m)^\[workspace\.package]\s*$([\s\S]*?)(?=^\[|\z)""").find(text)?.groupValues?.get(1)
    val match = section?.let { Regex("""(?m)^version\s*=\s*"(\d+)\.(\d+)\.(\d+)"""").find(it) }
        ?: throw GradleException("Cannot read X.Y.Z from [workspace.package] version in $cargoToml")
    match.groupValues.drop(1).joinToString(".")
}
val workspaceVersionCode: Int = workspaceVersion.split(".").map { it.toInt() }.let { (major, minor, patch) ->
    major * 10000 + minor * 100 + patch
}

val releaseKeystoreFile: String? = System.getenv("HUBBRIDGE_KEYSTORE_FILE")?.takeIf { it.isNotEmpty() }
val releaseKeystorePassword: String? = System.getenv("HUBBRIDGE_KEYSTORE_PASSWORD")?.takeIf { it.isNotEmpty() }
val releaseKeyAlias: String? = System.getenv("HUBBRIDGE_KEY_ALIAS")?.takeIf { it.isNotEmpty() }
val releaseKeyPassword: String? = System.getenv("HUBBRIDGE_KEY_PASSWORD")?.takeIf { it.isNotEmpty() }
val hasReleaseKey = releaseKeystoreFile != null && releaseKeystorePassword != null &&
    releaseKeyAlias != null && releaseKeyPassword != null

android {
    namespace = "com.demoz.hubbridge"
    compileSdk = 36
    ndkVersion = "27.2.12479018"

    defaultConfig {
        applicationId = "com.demoz.hubbridge"
        minSdk = 26
        targetSdk = 36
        versionCode = workspaceVersionCode
        versionName = workspaceVersion
    }

    signingConfigs {
        if (hasReleaseKey) {
            create("release") {
                storeFile = file(releaseKeystoreFile!!)
                storePassword = releaseKeystorePassword
                keyAlias = releaseKeyAlias
                keyPassword = releaseKeyPassword
            }
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            signingConfig = signingConfigs.getByName(if (hasReleaseKey) "release" else "debug")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
}

if (!hasReleaseKey) {
    gradle.taskGraph.whenReady {
        if (allTasks.any { it.project == project && (it.name == "packageRelease" || it.name == "signReleaseBundle") }) {
            logger.warn(
                "WARNING: HUBBRIDGE_KEYSTORE_* variables are not all set, the release build is signed with the debug key",
            )
        }
    }
}

dependencies {
    // `./gradlew testDebugUnitTest`: the parts of the app without Android types.
    testImplementation("junit:junit:4.13.2")
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
