import org.gradle.external.javadoc.StandardJavadocDocletOptions

plugins {
  `java-library`
}

group = "dev.yesnodb"
version = "0.1.0-SNAPSHOT"

java {
  sourceCompatibility = JavaVersion.VERSION_17
  targetCompatibility = JavaVersion.VERSION_17
  withSourcesJar()
  withJavadocJar()
}

dependencies {
  api("dev.yesnodb:yesno-flight-client:0.1.0-SNAPSHOT")
  compileOnly("org.checkerframework:checker-qual:3.49.5")

  testCompileOnly("org.checkerframework:checker-qual:3.49.5")
  testImplementation("org.junit.jupiter:junit-jupiter:5.14.1")
  testRuntimeOnly("org.junit.platform:junit-platform-launcher:1.14.1")
}

val e2eTestSourceSet = sourceSets.create("e2eTest")
e2eTestSourceSet.compileClasspath += sourceSets.main.get().output
e2eTestSourceSet.runtimeClasspath += sourceSets.main.get().output
configurations[e2eTestSourceSet.implementationConfigurationName]
  .extendsFrom(configurations.testImplementation.get())
configurations[e2eTestSourceSet.runtimeOnlyConfigurationName]
  .extendsFrom(configurations.testRuntimeOnly.get())

tasks.withType<JavaCompile>().configureEach {
  options.encoding = "UTF-8"
  options.release = 17
  options.compilerArgs.addAll(listOf("-Xlint:all", "-Werror"))
}

tasks.test {
  useJUnitPlatform()
  jvmArgs("--add-opens=java.base/java.nio=ALL-UNNAMED")
}

tasks.register<Test>("e2eTest") {
  description = "Tests the application helpers against a real yesnodb Flight endpoint."
  group = "verification"
  testClassesDirs = e2eTestSourceSet.output.classesDirs
  classpath = e2eTestSourceSet.runtimeClasspath
  useJUnitPlatform()
  jvmArgs("--add-opens=java.base/java.nio=ALL-UNNAMED")

  val location = providers.gradleProperty("yesnoE2eFlightLocation")
  inputs.property("yesnoE2eFlightLocation", location)
  doFirst {
    systemProperty(
      "yesno.e2e.flight.location",
      location.orNull
        ?: throw GradleException("e2eTest requires -PyesnoE2eFlightLocation=grpc+tcp://HOST:PORT"),
    )
  }
}

tasks.javadoc {
  isFailOnError = true
  (options as StandardJavadocDocletOptions)
    .addBooleanOption("Xdoclint:all,-missing", true)
}

tasks.check {
  dependsOn(tasks.javadoc)
}
