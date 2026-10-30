idf_component_register(SRCS "esp_ot_br.c" "watchdog.c" "watchdog_core.c" "watchdog_matter.c"
                       INCLUDE_DIRS "."
                       EMBED_TXTFILES ${project_dir}/server_certs/ca_cert.pem
                       )
