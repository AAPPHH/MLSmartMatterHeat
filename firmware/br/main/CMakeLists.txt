set(srcs "esp_ot_br.c" "watchdog.c" "watchdog_core.c" "watchdog_matter.c")
if(NOT CONFIG_BR_WATCHDOG_MATTER_RUST)
    list(APPEND srcs "matter_proto.c" "matter_client.c")
endif()
idf_component_register(SRCS ${srcs}
                       INCLUDE_DIRS "."
                       EMBED_TXTFILES ${project_dir}/server_certs/ca_cert.pem
                       )
target_link_libraries(${COMPONENT_LIB} INTERFACE "-Wl,--wrap=esp_panic_handler")
if(CONFIG_BR_WATCHDOG_MATTER_RUST)
    get_filename_component(rs_dir "${CMAKE_CURRENT_LIST_DIR}/../matter_client" ABSOLUTE)
    set(rs_target_dir "${CMAKE_CURRENT_BINARY_DIR}/matter_rs")
    set(rs_lib "${rs_target_dir}/xtensa-esp32s3-none-elf/release/libmatter_client.a")
    file(GLOB rs_sources "${rs_dir}/src/*.rs")
    add_custom_command(OUTPUT "${rs_lib}"
                       COMMAND ${CMAKE_COMMAND} -E env RUSTUP_TOOLCHAIN=esp CARGO_TARGET_DIR=${rs_target_dir}
                               cargo rustc --release --features ffi --target xtensa-esp32s3-none-elf -Zbuild-std=core --crate-type staticlib
                       WORKING_DIRECTORY "${rs_dir}"
                       DEPENDS ${rs_sources} "${rs_dir}/Cargo.toml" "${rs_dir}/Cargo.lock"
                       VERBATIM)
    add_custom_target(matter_rs_build DEPENDS "${rs_lib}")
    add_library(matter_rs STATIC IMPORTED GLOBAL)
    set_target_properties(matter_rs PROPERTIES IMPORTED_LOCATION "${rs_lib}")
    add_dependencies(matter_rs matter_rs_build)
    add_dependencies(${COMPONENT_LIB} matter_rs_build)
    target_link_libraries(${COMPONENT_LIB} PRIVATE matter_rs)
endif()
