@file:Suppress("UnstableApiUsage")

import com.android.build.api.variant.FilterConfiguration.FilterType.ABI
import com.android.build.gradle.internal.cxx.configure.gradleLocalProperties
import com.github.willir.rust.CargoNdkBuildTask
import org.jetbrains.kotlin.gradle.dsl.JvmTarget
import org.jetbrains.kotlin.konan.properties.hasProperty
import org.jetbrains.kotlin.konan.properties.propertyList
import java.util.Properties

val localProperties = gradleLocalProperties(rootDir, providers)
val abiFilterList = ((localProperties["ABI_FILTERS"] ?: properties["ABI_FILTERS"]) as? String)
    ?.split(';')
val abiCodes = mapOf("armeabi-v7a" to 1, "arm64-v8a" to 2, "x86" to 3, "x86_64" to 4)

// 内部测试签名（三个分支共用同一把 key，便于覆盖安装与统一分发）
val keystoreProps = Properties().apply {
    val f = rootProject.file("keystore.properties")
    if (f.exists()) f.inputStream().use { load(it) }
}

plugins {
    alias(libs.plugins.androidApplication)
    alias(libs.plugins.jetbrainsKotlinAndroid)
    alias(libs.plugins.cargoNdkAndroid)
    alias(libs.plugins.composeCompiler)
}

android {
    namespace = "rs.ruffle"
    compileSdk = 36

    defaultConfig {
        applicationId = "com.sxd.sxdffle.mod"
        minSdk = 26
        targetSdk = 35
        versionCode = 261154
        versionName = "0.261154-mod"

        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
        vectorDrawables {
            useSupportLibrary = true
        }

        ndk {
            if (abiFilterList == null) {
                abiFilters.addAll(listOf("armeabi-v7a", "arm64-v8a", "x86", "x86_64"))
            }
        }
    }

    signingConfigs {
        create("release") {
            if (keystoreProps.isNotEmpty()) {
                storeFile = rootProject.file(keystoreProps.getProperty("storeFile"))
                storePassword = keystoreProps.getProperty("storePassword")
                keyAlias = keystoreProps.getProperty("keyAlias")
                keyPassword = keystoreProps.getProperty("keyPassword")
            }
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro"
            )
            signingConfig = signingConfigs.findByName("release")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_1_8
        targetCompatibility = JavaVersion.VERSION_1_8
    }

    buildFeatures {
        compose = true
        prefab = true
    }

    packaging {
        resources {
            excludes += "/META-INF/{AL2.0,LGPL2.1}"
        }
        jniLibs {
            // 引擎库 libsxdffle.so 约 22MB，默认以 Stored（不压缩）方式打包，
            // 会直接让 APK 膨胀 14MB 左右。开启 legacy 打包方式后会被压缩，
            // 代价是安装时需解压到应用目录（对普通用户更友好的是体积小）。
            useLegacyPackaging = true
        }
    }

    splits {
        // Configures multiple APKs based on ABI.
        abi {
            // Enables building multiple APKs per ABI.
            isEnable = true

            // Resets the list of ABIs that Gradle should create APKs for to none.
            reset()

            // Specifies a list of ABIs that Gradle should create APKs for.
            if (abiFilterList != null && abiFilterList.isNotEmpty()) {
                include(*abiFilterList.toTypedArray())
            } else {
                include("armeabi-v7a", "arm64-v8a", "x86", "x86_64")

                // Specifies that we also want to generate a universal APK that includes all ABIs.
                isUniversalApk = true
            }
        }
    }
    dependenciesInfo {
        // Disables dependency metadata when building APKs.
        includeInApk = false
        // Disables dependency metadata when building Android App Bundles.
        includeInBundle = false
    }
}

kotlin {
    compilerOptions {
        jvmTarget.set(JvmTarget.JVM_1_8)
    }
}

androidComponents {
    onVariants { variant ->
        variant.outputs.forEach { output ->
            val name = output.filters.find { it.filterType == ABI }?.identifier
            val abiCode = abiCodes[name] ?: 0
            output.versionCode.set(output.versionCode.get() * 10 + abiCode)
        }
    }
}

dependencies {

    implementation(project(":rustls-platform-verifier"))
    implementation(libs.androidx.core.ktx)
    implementation(libs.androidx.lifecycle.runtime.ktx)
    implementation(libs.androidx.activity.compose)
    implementation(platform(libs.androidx.compose.bom))
    implementation(libs.androidx.ui)
    implementation(libs.androidx.ui.graphics)
    implementation(libs.androidx.ui.tooling.preview)
    implementation(libs.androidx.material3)
    implementation(libs.androidx.lifecycle.viewmodel.compose)
    implementation(libs.androidx.navigation.runtime.ktx)
    implementation(libs.androidx.navigation.compose)
    implementation(libs.androidx.games.activity)
    implementation(libs.androidx.constraintlayout)
    implementation(libs.androidx.appcompat)
    // SXDffle：document-start 脚本注入（伪造 Flash 插件检测）
    implementation(libs.androidx.webkit)
    androidTestImplementation(libs.androidx.uiautomator)
    androidTestImplementation(libs.androidx.test.runner)
    androidTestImplementation(libs.androidx.test.rules)
    testImplementation(libs.junit)
    androidTestImplementation(libs.androidx.junit)
    androidTestImplementation(libs.androidx.espresso.core)
    androidTestImplementation(platform(libs.androidx.compose.bom))
    androidTestImplementation(libs.androidx.ui.test.junit4)
    debugImplementation(libs.androidx.ui.tooling)
    debugImplementation(libs.androidx.ui.test.manifest)
}

// 原生库有两条产出路径：
//  1) 默认：由本插件在 Gradle 构建过程中调用 cargo-ndk 现场编译；
//  2) 预编译：先执行 scripts/build-rust.sh，产物落到 app/src/main/jniLibs，
//     然后设置 SXDFFLE_PREBUILT_NATIVE=1 让本插件跳过，避免重复编译。
// 在 CI 或离线环境中第 2 条更稳、更快。
if (System.getenv("GITHUB_ACTIONS") != null ||
    System.getenv("SXDFFLE_PREBUILT_NATIVE") != null
) {
    tasks.withType<CargoNdkBuildTask> {
        enabled = false
    }
}

cargoNdk {
    module = "."
    apiLevel = 26
    buildType = "release"

    if (localProperties.hasProperty("ndkTargets")) {
        targets = ArrayList(localProperties.propertyList("ndkTargets"))
    }
}
