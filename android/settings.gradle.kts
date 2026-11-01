// MoleRuffle:国内网络到 dl.google.com / plugins.gradle.org 常 TLS 失败,用阿里云镜像。
pluginManagement {
    // cargo-ndk 插件的 marker 只在 gradle 插件门户;阿里云门户镜像不全(有 pom 无 jar)。
    // 直接把插件 id 映射到它的真实 maven 坐标(在阿里云 public / maven central 上有 jar),绕过 marker。
    resolutionStrategy {
        eachPlugin {
            if (requested.id.id == "com.gemwallet.cargo-ndk") {
                useModule("com.gemwallet.gradle:plugin:${requested.version}")
            }
        }
    }
    repositories {
        maven { url = uri("https://maven.aliyun.com/repository/public") }
        maven { url = uri("https://maven.aliyun.com/repository/google") }
        google()
        mavenCentral()
        gradlePluginPortal()
    }
}
dependencyResolutionManagement {
    repositoriesMode.set(RepositoriesMode.FAIL_ON_PROJECT_REPOS)
    repositories {
        maven { url = uri("https://maven.aliyun.com/repository/public") }
        maven { url = uri("https://maven.aliyun.com/repository/google") }
        google()
        mavenCentral()
    }
}

rootProject.name = "Ruffle"
include(":app")
