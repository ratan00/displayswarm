plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "com.displayswarm.client"
    compileSdk = 34

    defaultConfig {
        applicationId = "com.displayswarm.client"
        minSdk = 26
        targetSdk = 34
        versionCode = 3
        versionName = "0.3.0"
        ndk {
            // Phones (64- and 32-bit ARM) and the x86_64 emulator.
            abiFilters += listOf("arm64-v8a", "armeabi-v7a", "x86_64")
        }
    }
    ndkVersion = "27.0.12077973"
    externalNativeBuild {
        cmake {
            path = file("src/main/cpp/CMakeLists.txt")
            version = "3.22.1"
        }
    }

    // Release signing comes from the environment so no secret is in the repo:
    // DS_KEYSTORE, DS_STORE_PASSWORD, DS_KEY_ALIAS, DS_KEY_PASSWORD. Unset = unsigned.
    val dsKeystore = System.getenv("DS_KEYSTORE")
    signingConfigs {
        if (dsKeystore != null) {
            create("release") {
                storeFile = file(dsKeystore)
                storePassword = System.getenv("DS_STORE_PASSWORD")
                keyAlias = System.getenv("DS_KEY_ALIAS")
                keyPassword = System.getenv("DS_KEY_PASSWORD")
            }
        }
    }

    buildTypes {
        release {
            if (dsKeystore != null) signingConfig = signingConfigs.getByName("release")
            // R8: the debug build users were given ran debuggable and unoptimised
            // (~75% of a core for audio alone), and material-icons-extended alone
            // is most of an 18 MB APK; shrinking keeps only the icons in use.
            isMinifyEnabled = true
            isShrinkResources = true
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro"
            )
        }
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions {
        jvmTarget = "17"
    }
    testOptions {
        unitTests.isReturnDefaultValues = true
    }
    buildFeatures {
        viewBinding = true
        compose = true
    }
    composeOptions {
        kotlinCompilerExtensionVersion = "1.5.10"
    }
}

dependencies {
    implementation("androidx.core:core-ktx:1.12.0")
    implementation("androidx.appcompat:appcompat:1.6.1")
    implementation("com.google.android.material:material:1.11.0")
    implementation("androidx.constraintlayout:constraintlayout:2.1.4")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.7.3")
    implementation(platform("androidx.compose:compose-bom:2024.02.00"))
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.material3:material3")
    implementation("androidx.compose.material:material-icons-extended")
    implementation("androidx.compose.foundation:foundation")
    implementation("androidx.activity:activity-compose:1.8.2")
    implementation("androidx.input:input-motionprediction:1.0.0-beta05")
    testImplementation("junit:junit:4.13.2")
}
