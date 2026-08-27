# SPDX-License-Identifier: MIT OR Apache-2.0
cmake_minimum_required(VERSION 3.20)
project(yesno_flight_cpp VERSION 0.1.0 LANGUAGES CXX)

option(YESNO_FLIGHT_CPP_BUILD_TESTS
       "Build hermetic unit tests and the live-server roundtrip client" ON)

find_package(Arrow CONFIG REQUIRED)
find_package(ArrowFlight CONFIG REQUIRED)

if(TARGET ArrowFlight::arrow_flight_shared)
  set(YESNO_ARROW_FLIGHT_TARGET ArrowFlight::arrow_flight_shared)
elseif(TARGET ArrowFlight::arrow_flight_static)
  set(YESNO_ARROW_FLIGHT_TARGET ArrowFlight::arrow_flight_static)
else()
  message(FATAL_ERROR "ArrowFlight package exports no supported Flight target")
endif()

add_library(yesno_flight_cpp STATIC src/client.cc src/protocol_internal.cc)
add_library(yesno::flight_cpp ALIAS yesno_flight_cpp)
target_compile_features(yesno_flight_cpp PUBLIC cxx_std_20)
set_target_properties(yesno_flight_cpp PROPERTIES POSITION_INDEPENDENT_CODE ON)
target_include_directories(
  yesno_flight_cpp
  PUBLIC
    $<BUILD_INTERFACE:${CMAKE_CURRENT_SOURCE_DIR}/include>
    $<INSTALL_INTERFACE:include>)
target_link_libraries(yesno_flight_cpp PUBLIC ${YESNO_ARROW_FLIGHT_TARGET})

if(YESNO_FLIGHT_CPP_BUILD_TESTS)
  include(CTest)
  add_executable(yesno_flight_cpp_unit tests/unit.cc)
  target_compile_features(yesno_flight_cpp_unit PRIVATE cxx_std_20)
  target_include_directories(
    yesno_flight_cpp_unit PRIVATE "${CMAKE_CURRENT_SOURCE_DIR}/src")
  target_link_libraries(yesno_flight_cpp_unit PRIVATE yesno_flight_cpp)
  add_test(NAME yesno_flight_cpp_unit COMMAND yesno_flight_cpp_unit)

  add_executable(yesno_flight_cpp_roundtrip tests/roundtrip.cc)
  target_compile_features(yesno_flight_cpp_roundtrip PRIVATE cxx_std_20)
  target_link_libraries(yesno_flight_cpp_roundtrip PRIVATE yesno_flight_cpp)
endif()

include(GNUInstallDirs)
install(TARGETS yesno_flight_cpp EXPORT yesno-flight-cpp-targets
        ARCHIVE DESTINATION ${CMAKE_INSTALL_LIBDIR})
install(DIRECTORY include/ DESTINATION ${CMAKE_INSTALL_INCLUDEDIR})
install(EXPORT yesno-flight-cpp-targets
        NAMESPACE yesno::
        DESTINATION ${CMAKE_INSTALL_LIBDIR}/cmake/yesno-flight-cpp)
