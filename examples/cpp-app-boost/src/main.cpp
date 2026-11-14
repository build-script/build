#include <boost/program_options.hpp>

#include <iostream>
#include <string>

namespace options = boost::program_options;

int main(int argc, char** argv) {
    options::options_description description("options");
    description.add_options()
        ("help", "shows the options")
        ("name", options::value<std::string>()->default_value("build-script"), "who to greet")
        ("times", options::value<int>()->default_value(1), "how often");

    options::variables_map values;
    options::store(options::parse_command_line(argc, argv, description), values);
    options::notify(values);

    if (values.count("help")) {
        std::cout << description << "\n";
        return 0;
    }

    for (int i = 0; i < values["times"].as<int>(); i++) {
        std::cout << "Hello, " << values["name"].as<std::string>() << "\n";
    }

    return 0;
}
