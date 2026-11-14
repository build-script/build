#include <curl/curl.h>
#include <nlohmann/json.hpp>

#include <iostream>
#include <string>
#include <utility>

int main(int argc, char** argv) {
    std::string url = argc > 1 ? argv[1] : "https://example.com:8443/path/to/page?query=1#top";
    CURLU* handle = curl_url();

    if (curl_url_set(handle, CURLUPART_URL, url.c_str(), 0) != CURLUE_OK) {
        std::cerr << "not a URL: " << url << "\n";
        return 1;
    }

    const std::pair<const char*, CURLUPart> parts_of_url[] = {
        {"scheme", CURLUPART_SCHEME}, {"host", CURLUPART_HOST}, {"port", CURLUPART_PORT},
        {"path", CURLUPART_PATH}, {"query", CURLUPART_QUERY}, {"fragment", CURLUPART_FRAGMENT},
    };

    nlohmann::json parts;

    for (const auto& [name, part] : parts_of_url) {
        char* value = nullptr;

        if (curl_url_get(handle, part, &value, 0) == CURLUE_OK) {
            parts[name] = value;
            curl_free(value);
        }
    }

    curl_url_cleanup(handle);
    std::cout << parts.dump(2) << "\n" << curl_version() << "\n";
    return 0;
}
