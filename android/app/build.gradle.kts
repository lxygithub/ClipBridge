plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("org.jetbrains.kotlin.plugin.compose")
}

android {
    namespace = "top.ieop.clipbridge"
    compileSdk = 35

    defaultConfig {
        applicationId = "top.ieop.clipbridge"
        minSdk = 26
        targetSdk = 35
        versionCode = 1
        versionName = "0.1.0"

        // 预置电脑端 EndpointId：装完即配对，不需要手动抄一串 hex。
        // 可用 -Pclipbridge.peer=<id> 覆盖（打包给别人用时不带这个默认值）。
        buildConfigField(
            "String",
            "DEFAULT_PEER_ID",
            "\"${project.findProperty("clipbridge.peer") ?: "47127edd3a351b5dd3d922fce9cd7a2d3c86620cb6eb0b35f1805f2c88b7751c"}\""
        )
    }

    buildFeatures {
        compose = true
        buildConfig = true
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    kotlinOptions {
        jvmTarget = "17"
    }
}

dependencies {
    implementation("androidx.core:core-ktx:1.13.1")
    implementation("androidx.activity:activity-compose:1.9.2")
    implementation(platform("androidx.compose:compose-bom:2024.09.03"))
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.material3:material3")
    implementation("androidx.work:work-runtime-ktx:2.9.1")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.8.1")
}
