#pragma once

#include <string>
#include <utility>
#include <vector>

namespace core {

std::string greeting(const std::string& name);
std::string table(const std::vector< std::pair<std::string, int> >& rows);

}
