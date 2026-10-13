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

// The live-daemon check: a `main` that drives this client against a real
// `yesnod` and prints what it saw, for the Monty scenario to assert on.
//
// A source set rather than a test source set, and a `main` rather than a JUnit
// test, because of where it runs. The E2E image's Gradle dependency cache is a
// BuildKit cache mount, so it exists while the image is being built and is gone
// by the time a gate runs inside it. Anything that needs Gradle at test time
// would reach Maven Central from inside a gate, which is the wrong shape for a
// gate. `liveCheckInstall` below stages the classes and the whole runtime
// classpath into the image, and the harness then runs a plain `java -cp`.
val liveCheck = sourceSets.create("liveCheck")
liveCheck.compileClasspath += sourceSets.main.get().output
liveCheck.runtimeClasspath += sourceSets.main.get().output
configurations[liveCheck.implementationConfigurationName]
  .extendsFrom(configurations.implementation.get())
configurations[liveCheck.runtimeOnlyConfigurationName].extendsFrom(configurations.runtimeOnly.get())
// `-Werror` is on for every compilation here, so the nullness annotations the
// main classes carry must still resolve when this source set compiles against
// them.
configurations[liveCheck.compileOnlyConfigurationName].extendsFrom(configurations.compileOnly.get())

tasks.register<Sync>("liveCheckInstall") {
  description = "Stages the live-daemon check and its runtime classpath for offline execution."
  group = "verification"
  into(layout.buildDirectory.dir("live-check"))
  from(configurations[liveCheck.runtimeClasspathConfigurationName]) { into("lib") }
  from(sourceSets.main.get().output) { into("classes") }
  from(liveCheck.output) { into("classes") }
}

tasks.javadoc {
  isFailOnError = true
  (options as StandardJavadocDocletOptions)
    .addBooleanOption("Xdoclint:all,-missing", true)
}

tasks.check {
  dependsOn(tasks.javadoc)
  // Compile the live check in the routine Java gate, which costs a javac run;
  // staging its ~100 MB of jars is the image build's job and not this one's.
  dependsOn(liveCheck.classesTaskName)
}
