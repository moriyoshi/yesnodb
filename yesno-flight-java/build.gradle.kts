import org.gradle.external.javadoc.StandardJavadocDocletOptions

plugins {
  `java-library`
}

group = "dev.yesnodb"
version = "0.1.0-SNAPSHOT"

repositories {
  mavenCentral()
}

java {
  sourceCompatibility = JavaVersion.VERSION_17
  targetCompatibility = JavaVersion.VERSION_17
  withSourcesJar()
  withJavadocJar()
}

dependencies {
  api("org.apache.arrow:flight-core:19.0.0")
  runtimeOnly("org.apache.arrow:arrow-memory-netty:19.0.0")
  implementation("com.google.protobuf:protobuf-java:4.33.4")
  compileOnly("org.checkerframework:checker-qual:3.49.5")
  api("org.roaringbitmap:roaringbitmap:1.6.6")

  testImplementation("org.junit.jupiter:junit-jupiter:5.14.1")
  testCompileOnly("org.checkerframework:checker-qual:3.49.5")
  testRuntimeOnly("org.junit.platform:junit-platform-launcher:1.14.1")
}

tasks.withType<JavaCompile>().configureEach {
  options.encoding = "UTF-8"
  options.release = 17
  options.compilerArgs.addAll(listOf("-Xlint:all", "-Werror"))
}

tasks.test {
  useJUnitPlatform()
  jvmArgs("--add-opens=java.base/java.nio=ALL-UNNAMED")
}

tasks.javadoc {
  isFailOnError = true
  (options as StandardJavadocDocletOptions)
    .addBooleanOption("Xdoclint:all,-missing", true)
}

tasks.check {
  dependsOn(tasks.javadoc)
}
