cmake_minimum_required(VERSION 3.16)
project(opencv5_dependency_smoke LANGUAGES CXX)

find_package(OpenCV 5.0.0 EXACT CONFIG REQUIRED)

add_executable(opencv5_dependency_smoke main.cpp)
target_compile_features(opencv5_dependency_smoke PRIVATE cxx_std_17)
target_include_directories(opencv5_dependency_smoke PRIVATE ${OpenCV_INCLUDE_DIRS})
target_link_libraries(opencv5_dependency_smoke PRIVATE ${OpenCV_LIBS})
