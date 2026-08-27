import org.gradle.api.file.DuplicatesStrategy
import org.gradle.api.tasks.bundling.Zip

plugins {
  java
  id("com.gradleup.shadow") version "9.2.2"
}

group = "dev.yesnodb"
version = "0.1.0-SNAPSHOT"

java {
  sourceCompatibility = JavaVersion.VERSION_21
  targetCompatibility = JavaVersion.VERSION_21
}

dependencies {
  compileOnly("org.opensearch:opensearch:3.8.0")
  implementation("dev.yesnodb:yesno-search-java:0.1.0-SNAPSHOT") {
    exclude(group = "org.apache.arrow", module = "arrow-memory-netty")
    exclude(group = "org.apache.arrow", module = "arrow-memory-netty-buffer-patch")
  }
  implementation("org.apache.arrow:arrow-memory-unsafe:19.0.0")
  compileOnly("org.checkerframework:checker-qual:3.49.5")

  testImplementation("org.junit.jupiter:junit-jupiter:5.14.1")
  testRuntimeOnly("org.junit.platform:junit-platform-launcher:1.14.1")
}

tasks.withType<JavaCompile>().configureEach {
  options.encoding = "UTF-8"
  options.release = 21
  options.compilerArgs.addAll(listOf("-Xlint:all", "-Werror"))
}

tasks.test {
  useJUnitPlatform()
}

tasks.shadowJar {
  archiveClassifier = ""
  mergeServiceFiles()
  exclude("META-INF/*.SF", "META-INF/*.DSA", "META-INF/*.RSA")

  relocate("org.apache.arrow", "dev.yesnodb.opensearch.internal.arrow")
  relocate("org.roaringbitmap", "dev.yesnodb.opensearch.internal.roaring")
  relocate("io.grpc", "dev.yesnodb.opensearch.internal.grpc")
  relocate("io.netty", "dev.yesnodb.opensearch.internal.netty")
  relocate("com.google.protobuf", "dev.yesnodb.opensearch.internal.protobuf")
  relocate("com.google.common", "dev.yesnodb.opensearch.internal.guava")
  relocate("com.google.gson", "dev.yesnodb.opensearch.internal.gson")
  relocate("com.google.flatbuffers", "dev.yesnodb.opensearch.internal.flatbuffers")
  relocate("com.google.errorprone", "dev.yesnodb.opensearch.internal.errorprone")
  relocate("com.google.j2objc", "dev.yesnodb.opensearch.internal.j2objc")
  relocate("com.fasterxml.jackson", "dev.yesnodb.opensearch.internal.jackson")
  relocate("org.apache.commons.codec", "dev.yesnodb.opensearch.internal.commonscodec")
  relocate("org.slf4j", "dev.yesnodb.opensearch.internal.slf4j")
  relocate("org.perfmark", "dev.yesnodb.opensearch.internal.perfmark")
  relocate("org.jspecify", "dev.yesnodb.opensearch.internal.jspecify")
  relocate("javax.annotation", "dev.yesnodb.opensearch.internal.javaxannotation")
  relocate("edu.umd.cs.findbugs", "dev.yesnodb.opensearch.internal.findbugs")
}

tasks.jar {
  enabled = false
}

val bundlePlugin = tasks.register<Zip>("bundlePlugin") {
  dependsOn(tasks.shadowJar)
  archiveBaseName = "yesno-opensearch-plugin"
  archiveVersion = "3.8.0-0.1.0-SNAPSHOT"
  destinationDirectory = layout.buildDirectory.dir("distributions")
  duplicatesStrategy = DuplicatesStrategy.FAIL

  from(tasks.shadowJar)
  from("src/main/plugin-metadata")
  if (providers.gradleProperty("yesnoE2ePermissions").isPresent) {
    from("src/e2e/plugin-metadata")
  }
}

tasks.assemble {
  dependsOn(bundlePlugin)
}
