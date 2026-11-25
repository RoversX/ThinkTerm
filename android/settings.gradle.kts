pluginManagement {
    repositories {
        google()
        mavenCentral()
        gradlePluginPortal()
    }
}
dependencyResolutionManagement {
    repositoriesMode.set(RepositoriesMode.FAIL_ON_PROJECT_REPOS)
    repositories {
        google()
        mavenCentral()
    }
}
rootProject.name = "ThinkTerm"
include(":app")
// The app lock (Face, fingerprint or the screen lock on opening), a module
// of its own: the app reaches it only through AppLock and AppLockGate.
include(":applock")
