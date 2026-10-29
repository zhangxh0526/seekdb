/*
 * Copyright (c) 2026 OceanBase.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

#include "sql/engine/basic/ob_file_sql_reader.h"
#include "lib/compress/zstd_1_3_8/ob_zstd_wrapper.h"

#include <algorithm>
#include <cerrno>
#include <charconv>
#include <cmath>
#include <cstdlib>
#include <cstring>
#include <fstream>
#include <iomanip>
#include <limits>
#include <sstream>
#ifndef _WIN32
#include <glob.h>
#endif

namespace oceanbase
{
namespace sql
{
namespace file_sql
{
namespace
{

int fail(Error &error, ErrorCode code, const std::string &message)
{
  error.code = code;
  error.message_ = message;
  return -1;
}

void append_u32(std::string &out, uint32_t value)
{
  for (int i = 0; i < 4; ++i) {
    out.push_back(static_cast<char>((value >> (i * 8)) & 0xff));
  }
}

void append_u64(std::string &out, uint64_t value)
{
  for (int i = 0; i < 8; ++i) {
    out.push_back(static_cast<char>((value >> (i * 8)) & 0xff));
  }
}

void append_string(std::string &out, const std::string &value)
{
  append_u32(out, static_cast<uint32_t>(value.size()));
  out.append(value);
}

uint32_t read_u32(const char *data)
{
  uint32_t value = 0;
  for (int i = 0; i < 4; ++i) {
    value |= static_cast<uint32_t>(static_cast<unsigned char>(data[i])) << (i * 8);
  }
  return value;
}

uint64_t read_u64(const char *data)
{
  uint64_t value = 0;
  for (int i = 0; i < 8; ++i) {
    value |= static_cast<uint64_t>(static_cast<unsigned char>(data[i])) << (i * 8);
  }
  return value;
}

bool take_string(const std::string &input, size_t &pos, std::string &value)
{
  if (pos + 4 > input.size()) {
    return false;
  }
  uint32_t len = read_u32(input.data() + pos);
  pos += 4;
  if (len > input.size() - pos) {
    return false;
  }
  value.assign(input.data() + pos, len);
  pos += len;
  return true;
}

int read_file(const std::string &path, std::string &data, Error &error)
{
  if (path.empty()) {
    return fail(error, ErrorCode::INVALID_ARGUMENT, "file path must not be empty");
  }
  std::ifstream input(path, std::ios::binary);
  if (!input) {
    return fail(error, ErrorCode::IO_ERROR, "cannot open file: " + path);
  }
  input.seekg(0, std::ios::end);
  std::streamoff size = input.tellg();
  if (size < 0) {
    return fail(error, ErrorCode::IO_ERROR, "cannot determine file size: " + path);
  }
  input.seekg(0, std::ios::beg);
  data.resize(static_cast<size_t>(size));
  if (size > 0 && !input.read(&data[0], size)) {
    return fail(error, ErrorCode::IO_ERROR, "cannot read file: " + path);
  }
  return 0;
}

bool iequals(const std::string &left, const std::string &right)
{
  if (left.size() != right.size()) {
    return false;
  }
  for (size_t i = 0; i < left.size(); ++i) {
    char a = left[i];
    char b = right[i];
    if (a >= 'A' && a <= 'Z') a = static_cast<char>(a - 'A' + 'a');
    if (b >= 'A' && b <= 'Z') b = static_cast<char>(b - 'A' + 'a');
    if (a != b) return false;
  }
  return true;
}

bool parse_integer(const std::string &text, int64_t &value)
{
  if (text.empty()) return false;
  const char *first = text.data();
  const char *last = first + text.size();
  auto result = std::from_chars(first, last, value);
  return result.ec == std::errc() && result.ptr == last;
}

bool parse_real(const std::string &text, double &value)
{
  if (text.empty()) return false;
  errno = 0;
  char *end = nullptr;
  value = std::strtod(text.c_str(), &end);
  return end == text.c_str() + text.size() && errno != ERANGE && std::isfinite(value);
}

bool leap_year(int year)
{
  return year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
}

int month_days(int year, int month)
{
  static const int days[] = {31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31};
  return month == 2 ? days[month - 1] + (leap_year(year) ? 1 : 0) : days[month - 1];
}

int64_t days_from_civil(int year, unsigned month, unsigned day)
{
  year -= month <= 2;
  const int era = (year >= 0 ? year : year - 399) / 400;
  const unsigned yoe = static_cast<unsigned>(year - era * 400);
  const unsigned doy = (153 * (month + (month > 2 ? -3 : 9)) + 2) / 5 + day - 1;
  const unsigned doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
  return static_cast<int64_t>(era) * 146097 + static_cast<int64_t>(doe) - 719468;
}

bool parse_date(const std::string &text, int64_t &days)
{
  if (text.size() != 10 || text[4] != '-' || text[7] != '-') return false;
  int year = 0, month = 0, day = 0;
  for (int i = 0; i < 10; ++i) {
    if (i == 4 || i == 7) continue;
    if (text[i] < '0' || text[i] > '9') return false;
  }
  year = std::stoi(text.substr(0, 4));
  month = std::stoi(text.substr(5, 2));
  day = std::stoi(text.substr(8, 2));
  if (year < 1 || month < 1 || month > 12 || day < 1 || day > month_days(year, month)) return false;
  days = days_from_civil(year, static_cast<unsigned>(month), static_cast<unsigned>(day));
  return true;
}

bool parse_datetime(const std::string &text, int64_t &micros)
{
  int64_t days = 0;
  if (text.size() < 19 || (text[10] != ' ' && text[10] != 'T') || !parse_date(text.substr(0, 10), days)) {
    return false;
  }
  if (text[13] != ':' || text[16] != ':') return false;
  for (size_t i = 11; i < 19; ++i) {
    if (i == 13 || i == 16) continue;
    if (text[i] < '0' || text[i] > '9') return false;
  }
  int hour = std::stoi(text.substr(11, 2));
  int minute = std::stoi(text.substr(14, 2));
  int second = std::stoi(text.substr(17, 2));
  if (hour > 23 || minute > 59 || second > 59) return false;
  int64_t fraction = 0;
  if (text.size() > 19) {
    if (text[19] != '.' || text.size() == 20 || text.size() > 26) return false;
    size_t digits = text.size() - 20;
    for (size_t i = 20; i < text.size(); ++i) {
      if (text[i] < '0' || text[i] > '9') return false;
      fraction = fraction * 10 + (text[i] - '0');
    }
    for (size_t i = digits; i < 6; ++i) fraction *= 10;
  }
  micros = (days * 86400 + hour * 3600 + minute * 60 + second) * 1000000 + fraction;
  return true;
}

ColumnType infer_text_type(const std::string &text)
{
  int64_t int_value = 0;
  double double_value = 0.0;
  if (parse_integer(text, int_value)) return ColumnType::BIGINT;
  if (parse_real(text, double_value)) return ColumnType::DOUBLE;
  if (iequals(text, "true") || iequals(text, "false")) return ColumnType::BOOLEAN;
  if (parse_date(text, int_value)) return ColumnType::DATE;
  if (parse_datetime(text, int_value)) return ColumnType::DATETIME;
  return ColumnType::VARCHAR;
}

ColumnType merge_types(ColumnType left, ColumnType right)
{
  if (left == right) return left;
  if ((left == ColumnType::BIGINT && right == ColumnType::DOUBLE) ||
      (left == ColumnType::DOUBLE && right == ColumnType::BIGINT)) return ColumnType::DOUBLE;
  if ((left == ColumnType::DATE && right == ColumnType::DATETIME) ||
      (left == ColumnType::DATETIME && right == ColumnType::DATE)) return ColumnType::DATETIME;
  return ColumnType::VARCHAR;
}

std::string double_to_string(double value)
{
  std::ostringstream out;
  out << std::setprecision(17) << value;
  return out.str();
}

std::string cell_to_text(const Cell &cell, ColumnType type)
{
  if (cell.is_null_) return std::string();
  switch (type) {
    case ColumnType::BIGINT:
    case ColumnType::DATE:
    case ColumnType::DATETIME:
      return std::to_string(cell.int_value_);
    case ColumnType::DOUBLE:
      return double_to_string(cell.double_value_);
    case ColumnType::BOOLEAN:
      return cell.int_value_ != 0 ? "true" : "false";
    case ColumnType::VARCHAR:
      return cell.string_value_;
  }
  return std::string();
}

int text_to_cell(const std::string &text, bool quoted, ColumnType type, Cell &cell, Error &error)
{
  if (text.empty() && !quoted) {
    cell = Cell::null();
    return 0;
  }
  int64_t int_value = 0;
  double double_value = 0.0;
  switch (type) {
    case ColumnType::VARCHAR:
      cell = Cell::text(text);
      return 0;
    case ColumnType::BIGINT:
      if (parse_integer(text, int_value)) {
        cell = Cell::integer(int_value);
        return 0;
      }
      break;
    case ColumnType::DOUBLE:
      if (parse_real(text, double_value)) {
        cell = Cell::real(double_value);
        return 0;
      }
      break;
    case ColumnType::BOOLEAN:
      if (iequals(text, "true")) {
        cell = Cell::boolean(true);
        return 0;
      } else if (iequals(text, "false")) {
        cell = Cell::boolean(false);
        return 0;
      }
      break;
    case ColumnType::DATE:
      if (parse_date(text, int_value)) {
        cell = Cell::temporal(ColumnType::DATE, int_value);
        return 0;
      }
      break;
    case ColumnType::DATETIME:
      if (parse_datetime(text, int_value) || (parse_date(text, int_value) && (int_value *= 86400000000LL, true))) {
        cell = Cell::temporal(ColumnType::DATETIME, int_value);
        return 0;
      }
      break;
  }
  return fail(error, ErrorCode::DATA_ERROR,
              "cannot convert value '" + text + "' to " + column_type_name(type));
}

struct CsvField
{
  std::string text_;
  bool quoted_ = false;
};
using CsvRecord = std::vector<CsvField>;

int parse_csv_records(const std::string &data, std::vector<CsvRecord> &records, Error &error)
{
  CsvRecord record;
  CsvField field;
  bool in_quotes = false;
  bool after_quote = false;
  bool record_has_input = false;
  auto finish_record = [&]() {
    record.push_back(field);
    if (record.size() != 1 || !record[0].text_.empty() || record[0].quoted_ || record_has_input) {
      records.push_back(record);
    }
    record.clear();
    field = CsvField();
    after_quote = false;
    record_has_input = false;
  };
  for (size_t i = 0; i < data.size(); ++i) {
    char c = data[i];
    record_has_input = true;
    if (in_quotes) {
      if (c == '"') {
        if (i + 1 < data.size() && data[i + 1] == '"') {
          field.text_.push_back('"');
          ++i;
        } else {
          in_quotes = false;
          after_quote = true;
        }
      } else if (c == '\r' && i + 1 < data.size() && data[i + 1] == '\n') {
        field.text_.push_back('\n');
        ++i;
      } else {
        field.text_.push_back(c);
      }
    } else if (after_quote) {
      if (c == ',') {
        record.push_back(field);
        field = CsvField();
        after_quote = false;
      } else if (c == '\n' || c == '\r') {
        if (c == '\r' && i + 1 < data.size() && data[i + 1] == '\n') ++i;
        finish_record();
      } else if (c != ' ' && c != '\t') {
        return fail(error, ErrorCode::DATA_ERROR, "unexpected characters after closing CSV quote");
      }
    } else if (c == '"' && field.text_.empty()) {
      in_quotes = true;
      field.quoted_ = true;
    } else if (c == ',') {
      record.push_back(field);
      field = CsvField();
    } else if (c == '\n' || c == '\r') {
      if (c == '\r' && i + 1 < data.size() && data[i + 1] == '\n') ++i;
      finish_record();
    } else {
      field.text_.push_back(c);
    }
  }
  if (in_quotes) {
    return fail(error, ErrorCode::DATA_ERROR, "unterminated quoted CSV field");
  }
  if (record_has_input || !record.empty() || !field.text_.empty() || field.quoted_) {
    finish_record();
  }
  return 0;
}

int read_csv(const Request &request, Table &table, Error &error)
{
  if (!request.format_.empty() && !iequals(request.format_, "csv")) {
    return fail(error, ErrorCode::INVALID_ARGUMENT, "unsupported CSV format: " + request.format_);
  }
  if (request.sample_size_ == 0 || request.sample_size_ < -1) {
    return fail(error, ErrorCode::INVALID_ARGUMENT, "sample_size must be -1 or a positive integer");
  }
  std::string data;
  if (read_file(request.path_, data, error) != 0) return -1;
  if (data.empty()) return fail(error, ErrorCode::DATA_ERROR, "cannot infer a schema from an empty CSV file");
  if (data.size() >= 3 && static_cast<unsigned char>(data[0]) == 0xef &&
      static_cast<unsigned char>(data[1]) == 0xbb && static_cast<unsigned char>(data[2]) == 0xbf) {
    data.erase(0, 3);
  }
  std::vector<CsvRecord> records;
  if (parse_csv_records(data, records, error) != 0) return -1;
  if (records.empty()) return fail(error, ErrorCode::DATA_ERROR, "cannot infer a schema from an empty CSV file");
  size_t data_start = request.header_ ? 1 : 0;
  if (request.header_ && records.size() == 1) data_start = 1;
  size_t column_count = records[0].size();
  if (column_count == 0) return fail(error, ErrorCode::DATA_ERROR, "CSV file has no columns");
  table.columns_.resize(column_count);
  for (size_t i = 0; i < column_count; ++i) {
    std::string base = request.header_ ? records[0][i].text_ : "column" + std::to_string(i);
    if (base.empty()) base = "column" + std::to_string(i);
    std::string name = base;
    int suffix = 1;
    bool duplicate = true;
    while (duplicate) {
      duplicate = false;
      for (size_t j = 0; j < i; ++j) {
        if (table.columns_[j].name_ == name) {
          duplicate = true;
          name = base + "__" + std::to_string(++suffix);
          break;
        }
      }
    }
    table.columns_[i].name_ = name;
  }
  std::vector<bool> has_value(column_count, false);
  size_t sample_end = records.size();
  if (request.sample_size_ > 0) {
    sample_end = std::min(sample_end, data_start + static_cast<size_t>(request.sample_size_));
  }
  for (size_t row_idx = data_start; row_idx < sample_end; ++row_idx) {
    if (records[row_idx].size() != column_count) {
      return fail(error, ErrorCode::DATA_ERROR, "CSV row has a different number of fields at row " + std::to_string(row_idx + 1));
    }
    for (size_t col = 0; col < column_count; ++col) {
      const CsvField &field = records[row_idx][col];
      if (field.text_.empty() && !field.quoted_) {
        table.columns_[col].nullable_ = true;
        continue;
      }
      ColumnType type = infer_text_type(field.text_);
      table.columns_[col].type_ = has_value[col] ? merge_types(table.columns_[col].type_, type) : type;
      has_value[col] = true;
    }
  }
  table.rows_.reserve(records.size() - data_start);
  for (size_t row_idx = data_start; row_idx < records.size(); ++row_idx) {
    if (records[row_idx].size() != column_count) {
      return fail(error, ErrorCode::DATA_ERROR, "CSV row has a different number of fields at row " + std::to_string(row_idx + 1));
    }
    std::vector<Cell> row(column_count);
    for (size_t col = 0; col < column_count; ++col) {
      const CsvField &field = records[row_idx][col];
      if (text_to_cell(field.text_, field.quoted_, table.columns_[col].type_, row[col], error) != 0) return -1;
      if (row[col].is_null_) table.columns_[col].nullable_ = true;
    }
    table.rows_.push_back(std::move(row));
  }
  return 0;
}

struct JsonValue
{
  enum class Type { NUL, BOOL, INT, DOUBLE, STRING } type_ = Type::NUL;
  bool bool_value_ = false;
  int64_t int_value_ = 0;
  double double_value_ = 0.0;
  std::string string_value_;
};
using JsonObject = std::vector<std::pair<std::string, JsonValue>>;

class JsonLineParser
{
public:
  JsonLineParser(const std::string &text, Error &error) : text_(text), error_(error) {}
  int parse(JsonObject &object)
  {
    skip_space();
    if (!consume('{')) return fail_at("JSONL row must be an object");
    skip_space();
    if (consume('}')) {
      skip_space();
      return pos_ == text_.size() ? 0 : fail_at("unexpected characters after JSON object");
    }
    while (pos_ < text_.size()) {
      std::string key;
      JsonValue value;
      if (parse_string(key) != 0) return -1;
      skip_space();
      if (!consume(':')) return fail_at("expected ':' after JSON object key");
      skip_space();
      if (parse_value(value) != 0) return -1;
      object.emplace_back(std::move(key), std::move(value));
      skip_space();
      if (consume('}')) {
        skip_space();
        return pos_ == text_.size() ? 0 : fail_at("unexpected characters after JSON object");
      }
      if (!consume(',')) return fail_at("expected ',' between JSON object fields");
      skip_space();
    }
    return fail_at("unterminated JSON object");
  }
private:
  void skip_space()
  {
    while (pos_ < text_.size() && (text_[pos_] == ' ' || text_[pos_] == '\t' || text_[pos_] == '\r' || text_[pos_] == '\n')) ++pos_;
  }
  bool consume(char c)
  {
    if (pos_ < text_.size() && text_[pos_] == c) {
      ++pos_;
      return true;
    }
    return false;
  }
  int fail_at(const std::string &message)
  {
    return fail(error_, ErrorCode::DATA_ERROR, message + " at byte " + std::to_string(pos_));
  }
  int parse_hex4(uint32_t &value)
  {
    if (pos_ + 4 > text_.size()) return fail_at("incomplete JSON unicode escape");
    value = 0;
    for (int i = 0; i < 4; ++i) {
      char c = text_[pos_++];
      value <<= 4;
      if (c >= '0' && c <= '9') value |= c - '0';
      else if (c >= 'a' && c <= 'f') value |= c - 'a' + 10;
      else if (c >= 'A' && c <= 'F') value |= c - 'A' + 10;
      else return fail_at("invalid JSON unicode escape");
    }
    return 0;
  }
  static void append_utf8(std::string &out, uint32_t cp)
  {
    if (cp <= 0x7f) out.push_back(static_cast<char>(cp));
    else if (cp <= 0x7ff) {
      out.push_back(static_cast<char>(0xc0 | (cp >> 6)));
      out.push_back(static_cast<char>(0x80 | (cp & 0x3f)));
    } else if (cp <= 0xffff) {
      out.push_back(static_cast<char>(0xe0 | (cp >> 12)));
      out.push_back(static_cast<char>(0x80 | ((cp >> 6) & 0x3f)));
      out.push_back(static_cast<char>(0x80 | (cp & 0x3f)));
    } else {
      out.push_back(static_cast<char>(0xf0 | (cp >> 18)));
      out.push_back(static_cast<char>(0x80 | ((cp >> 12) & 0x3f)));
      out.push_back(static_cast<char>(0x80 | ((cp >> 6) & 0x3f)));
      out.push_back(static_cast<char>(0x80 | (cp & 0x3f)));
    }
  }
  int parse_string(std::string &value)
  {
    if (!consume('"')) return fail_at("expected JSON string");
    value.clear();
    while (pos_ < text_.size()) {
      unsigned char c = static_cast<unsigned char>(text_[pos_++]);
      if (c == '"') return 0;
      if (c < 0x20) return fail_at("control character in JSON string");
      if (c != '\\') {
        value.push_back(static_cast<char>(c));
        continue;
      }
      if (pos_ >= text_.size()) return fail_at("incomplete JSON escape");
      char escaped = text_[pos_++];
      switch (escaped) {
        case '"': value.push_back('"'); break;
        case '\\': value.push_back('\\'); break;
        case '/': value.push_back('/'); break;
        case 'b': value.push_back('\b'); break;
        case 'f': value.push_back('\f'); break;
        case 'n': value.push_back('\n'); break;
        case 'r': value.push_back('\r'); break;
        case 't': value.push_back('\t'); break;
        case 'u': {
          uint32_t cp = 0;
          if (parse_hex4(cp) != 0) return -1;
          if (cp >= 0xd800 && cp <= 0xdbff && pos_ + 6 <= text_.size() && text_[pos_] == '\\' && text_[pos_ + 1] == 'u') {
            pos_ += 2;
            uint32_t low = 0;
            if (parse_hex4(low) != 0) return -1;
            if (low < 0xdc00 || low > 0xdfff) return fail_at("invalid JSON surrogate pair");
            cp = 0x10000 + ((cp - 0xd800) << 10) + (low - 0xdc00);
          } else if (cp >= 0xd800 && cp <= 0xdfff) {
            return fail_at("unpaired JSON surrogate");
          }
          append_utf8(value, cp);
          break;
        }
        default: return fail_at("invalid JSON escape");
      }
    }
    return fail_at("unterminated JSON string");
  }
  int skip_container(char open, char close)
  {
    int depth = 0;
    bool in_string = false;
    bool escaped = false;
    while (pos_ < text_.size()) {
      char c = text_[pos_++];
      if (in_string) {
        if (escaped) escaped = false;
        else if (c == '\\') escaped = true;
        else if (c == '"') in_string = false;
      } else if (c == '"') {
        in_string = true;
      } else if (c == open) {
        ++depth;
      } else if (c == close && --depth == 0) {
        return 0;
      }
    }
    return fail_at("unterminated JSON container");
  }
  int parse_value(JsonValue &value)
  {
    if (pos_ >= text_.size()) return fail_at("missing JSON value");
    size_t start = pos_;
    if (text_[pos_] == '"') {
      value.type_ = JsonValue::Type::STRING;
      return parse_string(value.string_value_);
    }
    if (text_.compare(pos_, 4, "null") == 0) {
      pos_ += 4;
      value.type_ = JsonValue::Type::NUL;
      return 0;
    }
    if (text_.compare(pos_, 4, "true") == 0) {
      pos_ += 4;
      value.type_ = JsonValue::Type::BOOL;
      value.bool_value_ = true;
      return 0;
    }
    if (text_.compare(pos_, 5, "false") == 0) {
      pos_ += 5;
      value.type_ = JsonValue::Type::BOOL;
      value.bool_value_ = false;
      return 0;
    }
    if (text_[pos_] == '{' || text_[pos_] == '[') {
      char open = text_[pos_];
      if (skip_container(open, open == '{' ? '}' : ']') != 0) return -1;
      value.type_ = JsonValue::Type::STRING;
      value.string_value_ = text_.substr(start, pos_ - start);
      return 0;
    }
    bool numeric = false;
    bool real = false;
    while (pos_ < text_.size()) {
      char c = text_[pos_];
      if ((c >= '0' && c <= '9') || c == '-' || c == '+') {
        numeric = true;
        ++pos_;
      } else if (c == '.' || c == 'e' || c == 'E') {
        numeric = true;
        real = true;
        ++pos_;
      } else {
        break;
      }
    }
    if (!numeric) return fail_at("invalid JSON value");
    std::string number = text_.substr(start, pos_ - start);
    if (real) {
      value.type_ = JsonValue::Type::DOUBLE;
      if (!parse_real(number, value.double_value_)) return fail_at("invalid JSON number");
    } else {
      value.type_ = JsonValue::Type::INT;
      if (!parse_integer(number, value.int_value_)) {
        value.type_ = JsonValue::Type::DOUBLE;
        if (!parse_real(number, value.double_value_)) return fail_at("invalid JSON number");
      }
    }
    return 0;
  }
  const std::string &text_;
  size_t pos_ = 0;
  Error &error_;
};

ColumnType json_type(const JsonValue &value)
{
  switch (value.type_) {
    case JsonValue::Type::BOOL: return ColumnType::BOOLEAN;
    case JsonValue::Type::INT: return ColumnType::BIGINT;
    case JsonValue::Type::DOUBLE: return ColumnType::DOUBLE;
    case JsonValue::Type::STRING: return ColumnType::VARCHAR;
    case JsonValue::Type::NUL: break;
  }
  return ColumnType::VARCHAR;
}

Cell json_cell(const JsonValue *value)
{
  if (value == nullptr || value->type_ == JsonValue::Type::NUL) return Cell::null();
  switch (value->type_) {
    case JsonValue::Type::BOOL: return Cell::boolean(value->bool_value_);
    case JsonValue::Type::INT: return Cell::integer(value->int_value_);
    case JsonValue::Type::DOUBLE: return Cell::real(value->double_value_);
    case JsonValue::Type::STRING: return Cell::text(value->string_value_);
    case JsonValue::Type::NUL: break;
  }
  return Cell::null();
}

int read_json(const Request &request, Table &table, Error &error)
{
  if (!request.format_.empty() && !iequals(request.format_, "newline_delimited") && !iequals(request.format_, "json")) {
    return fail(error, ErrorCode::INVALID_ARGUMENT, "only newline_delimited JSON format is supported");
  }
  std::string data;
  if (read_file(request.path_, data, error) != 0) return -1;
  if (data.empty()) return fail(error, ErrorCode::DATA_ERROR, "cannot infer a schema from an empty JSONL file");
  std::vector<JsonObject> objects;
  size_t start = 0;
  while (start < data.size()) {
    size_t end = data.find('\n', start);
    if (end == std::string::npos) end = data.size();
    std::string line = data.substr(start, end - start);
    if (!line.empty() && line.back() == '\r') line.pop_back();
    bool blank = true;
    for (char c : line) {
      if (c != ' ' && c != '\t') { blank = false; break; }
    }
    if (!blank) {
      JsonObject object;
      JsonLineParser parser(line, error);
      if (parser.parse(object) != 0) return -1;
      objects.push_back(std::move(object));
    }
    start = end + 1;
  }
  if (objects.empty()) return fail(error, ErrorCode::DATA_ERROR, "cannot infer a schema from an empty JSONL file");
  std::vector<bool> has_value;
  for (const JsonObject &object : objects) {
    for (const auto &entry : object) {
      size_t col = 0;
      while (col < table.columns_.size() && table.columns_[col].name_ != entry.first) ++col;
      if (col == table.columns_.size()) {
        Column column;
        column.name_ = entry.first;
        column.type_ = json_type(entry.second);
        table.columns_.push_back(column);
        has_value.push_back(entry.second.type_ != JsonValue::Type::NUL);
      } else if (entry.second.type_ != JsonValue::Type::NUL) {
        table.columns_[col].type_ = has_value[col] ? merge_types(table.columns_[col].type_, json_type(entry.second)) : json_type(entry.second);
        has_value[col] = true;
      }
      if (entry.second.type_ == JsonValue::Type::NUL) table.columns_[col].nullable_ = true;
    }
  }
  for (const JsonObject &object : objects) {
    std::vector<Cell> row(table.columns_.size());
    for (size_t col = 0; col < table.columns_.size(); ++col) {
      const JsonValue *value = nullptr;
      for (const auto &entry : object) {
        if (entry.first == table.columns_[col].name_) value = &entry.second;
      }
      row[col] = json_cell(value);
      if (table.columns_[col].type_ == ColumnType::VARCHAR && !row[col].is_null_ && value->type_ != JsonValue::Type::STRING) {
        row[col] = Cell::text(cell_to_text(row[col], json_type(*value)));
      } else if (table.columns_[col].type_ == ColumnType::DOUBLE && !row[col].is_null_ && value->type_ == JsonValue::Type::INT) {
        row[col] = Cell::real(static_cast<double>(row[col].int_value_));
      }
      if (row[col].is_null_) table.columns_[col].nullable_ = true;
    }
    table.rows_.push_back(std::move(row));
  }
  return 0;
}

int read_glob(const Request &request, Table &table, Error &error)
{
#ifdef _WIN32
  return fail(error, ErrorCode::NOT_SUPPORTED, "glob is not supported on this platform");
#else
  Column column;
  column.name_ = "file";
  column.type_ = ColumnType::VARCHAR;
  table.columns_.push_back(column);
  glob_t matches;
  std::memset(&matches, 0, sizeof(matches));
  int rc = glob(request.path_.c_str(), 0, nullptr, &matches);
  if (rc == GLOB_NOMATCH) {
    globfree(&matches);
    return 0;
  }
  if (rc != 0) {
    globfree(&matches);
    return fail(error, ErrorCode::IO_ERROR, "invalid or unreadable glob pattern: " + request.path_);
  }
  for (size_t i = 0; i < matches.gl_pathc; ++i) {
    table.rows_.push_back(std::vector<Cell>{Cell::text(matches.gl_pathv[i])});
  }
  globfree(&matches);
  return 0;
#endif
}

class CompactReader
{
public:
  CompactReader(const std::string &data, size_t pos = 0) : data_(data), pos_(pos) {}
  size_t position() const { return pos_; }
  bool read_varint(uint64_t &value)
  {
    value = 0;
    for (int shift = 0; shift < 64; shift += 7) {
      if (pos_ >= data_.size()) return false;
      uint8_t byte = static_cast<uint8_t>(data_[pos_++]);
      value |= static_cast<uint64_t>(byte & 0x7f) << shift;
      if ((byte & 0x80) == 0) return true;
    }
    return false;
  }
  bool read_int(int64_t &value)
  {
    uint64_t raw = 0;
    if (!read_varint(raw)) return false;
    value = static_cast<int64_t>((raw >> 1) ^ (~(raw & 1) + 1));
    return true;
  }
  bool read_binary(std::string &value)
  {
    uint64_t len = 0;
    if (!read_varint(len) || len > data_.size() - pos_) return false;
    value.assign(data_.data() + pos_, static_cast<size_t>(len));
    pos_ += static_cast<size_t>(len);
    return true;
  }
  bool read_field(int &id, int &type)
  {
    if (pos_ >= data_.size()) return false;
    uint8_t header = static_cast<uint8_t>(data_[pos_++]);
    type = header & 0x0f;
    if (type == 0) {
      id = 0;
      return true;
    }
    int delta = (header >> 4) & 0x0f;
    if (delta != 0) {
      id += delta;
      return true;
    }
    int64_t long_id = 0;
    if (!read_int(long_id)) return false;
    id = static_cast<int>(long_id);
    return true;
  }
  bool read_list(int &size, int &element_type)
  {
    if (pos_ >= data_.size()) return false;
    uint8_t header = static_cast<uint8_t>(data_[pos_++]);
    size = (header >> 4) & 0x0f;
    element_type = header & 0x0f;
    if (size == 15) {
      uint64_t long_size = 0;
      if (!read_varint(long_size) || long_size > std::numeric_limits<int>::max()) return false;
      size = static_cast<int>(long_size);
    }
    return true;
  }
  bool skip(int type)
  {
    uint64_t ignored = 0;
    std::string binary;
    switch (type) {
      case 1: case 2: return true;
      case 3: if (pos_ >= data_.size()) return false; ++pos_; return true;
      case 4: case 5: case 6: {
        int64_t value = 0;
        return read_int(value);
      }
      case 7: if (data_.size() - pos_ < 8) return false; pos_ += 8; return true;
      case 8: return read_binary(binary);
      case 9: case 10: {
        int size = 0, element_type = 0;
        if (!read_list(size, element_type)) return false;
        for (int i = 0; i < size; ++i) if (!skip(element_type)) return false;
        return true;
      }
      case 11: {
        if (!read_varint(ignored)) return false;
        if (ignored == 0) return true;
        if (pos_ >= data_.size()) return false;
        uint8_t types = static_cast<uint8_t>(data_[pos_++]);
        for (uint64_t i = 0; i < ignored; ++i) {
          if (!skip(types >> 4) || !skip(types & 0x0f)) return false;
        }
        return true;
      }
      case 12: {
        while (true) {
          int id = 0, field_type = 0;
          if (!read_field(id, field_type)) return false;
          if (field_type == 0) return true;
          if (!skip(field_type)) return false;
        }
      }
      default: return false;
    }
  }
private:
  const std::string &data_;
  size_t pos_;
};

struct ParquetSchemaElement
{
  int physical_type_ = -1;
  int repetition_type_ = -1;
  int converted_type_ = -1;
  std::string name_;
};

struct ParquetColumnChunk
{
  int physical_type_ = -1;
  int codec_ = 0;
  int64_t num_values_ = 0;
  int64_t data_page_offset_ = 0;
  int64_t dictionary_page_offset_ = 0;
  std::vector<std::string> path_;
};

struct ParquetRowGroup
{
  int64_t num_rows_ = 0;
  std::vector<ParquetColumnChunk> columns_;
};

struct ParquetMetadata
{
  int64_t num_rows_ = 0;
  std::vector<ParquetSchemaElement> schema_;
  std::vector<ParquetRowGroup> row_groups_;
};

bool parse_schema_element(CompactReader &reader, ParquetSchemaElement &element)
{
  int id = 0, type = 0;
  while (reader.read_field(id, type)) {
    if (type == 0) return true;
    int64_t value = 0;
    if (id == 1 || id == 3 || id == 6) {
      if (!reader.read_int(value)) return false;
      if (id == 1) element.physical_type_ = static_cast<int>(value);
      else if (id == 3) element.repetition_type_ = static_cast<int>(value);
      else element.converted_type_ = static_cast<int>(value);
    } else if (id == 4) {
      if (!reader.read_binary(element.name_)) return false;
    } else if (!reader.skip(type)) {
      return false;
    }
  }
  return false;
}

bool parse_column_metadata(CompactReader &reader, ParquetColumnChunk &chunk)
{
  int id = 0, type = 0;
  while (reader.read_field(id, type)) {
    if (type == 0) return true;
    int64_t value = 0;
    if (id == 1 || id == 4 || id == 5 || id == 9 || id == 11) {
      if (!reader.read_int(value)) return false;
      if (id == 1) chunk.physical_type_ = static_cast<int>(value);
      else if (id == 4) chunk.codec_ = static_cast<int>(value);
      else if (id == 5) chunk.num_values_ = value;
      else if (id == 9) chunk.data_page_offset_ = value;
      else chunk.dictionary_page_offset_ = value;
    } else if (id == 3) {
      int size = 0, element_type = 0;
      if (!reader.read_list(size, element_type) || element_type != 8) return false;
      for (int i = 0; i < size; ++i) {
        std::string name;
        if (!reader.read_binary(name)) return false;
        chunk.path_.push_back(name);
      }
    } else if (!reader.skip(type)) {
      return false;
    }
  }
  return false;
}

bool parse_column_chunk(CompactReader &reader, ParquetColumnChunk &chunk)
{
  int id = 0, type = 0;
  while (reader.read_field(id, type)) {
    if (type == 0) return true;
    if (id == 3 && type == 12) {
      if (!parse_column_metadata(reader, chunk)) return false;
    } else if (!reader.skip(type)) {
      return false;
    }
  }
  return false;
}

bool parse_row_group(CompactReader &reader, ParquetRowGroup &group)
{
  int id = 0, type = 0;
  while (reader.read_field(id, type)) {
    if (type == 0) return true;
    if (id == 1 && type == 9) {
      int size = 0, element_type = 0;
      if (!reader.read_list(size, element_type) || element_type != 12) return false;
      group.columns_.resize(size);
      for (int i = 0; i < size; ++i) if (!parse_column_chunk(reader, group.columns_[i])) return false;
    } else if (id == 3) {
      int64_t value = 0;
      if (!reader.read_int(value)) return false;
      group.num_rows_ = value;
    } else if (!reader.skip(type)) {
      return false;
    }
  }
  return false;
}

bool parse_metadata(const std::string &data, size_t footer_pos, ParquetMetadata &metadata)
{
  CompactReader reader(data, footer_pos);
  int id = 0, type = 0;
  while (reader.read_field(id, type)) {
    if (type == 0) return true;
    if (id == 2 && type == 9) {
      int size = 0, element_type = 0;
      if (!reader.read_list(size, element_type) || element_type != 12) return false;
      metadata.schema_.resize(size);
      for (int i = 0; i < size; ++i) if (!parse_schema_element(reader, metadata.schema_[i])) return false;
    } else if (id == 3) {
      if (!reader.read_int(metadata.num_rows_)) return false;
    } else if (id == 4 && type == 9) {
      int size = 0, element_type = 0;
      if (!reader.read_list(size, element_type) || element_type != 12) return false;
      metadata.row_groups_.resize(size);
      for (int i = 0; i < size; ++i) if (!parse_row_group(reader, metadata.row_groups_[i])) return false;
    } else if (!reader.skip(type)) {
      return false;
    }
  }
  return false;
}

struct ParquetPageHeader
{
  int page_type_ = -1;
  int32_t uncompressed_size_ = 0;
  int32_t compressed_size_ = 0;
  int32_t num_values_ = 0;
  int32_t num_nulls_ = 0;
  int32_t encoding_ = -1;
  int32_t definition_bytes_ = 0;
  int32_t repetition_bytes_ = 0;
  bool v2_ = false;
};

bool parse_page_header(CompactReader &reader, ParquetPageHeader &page)
{
  int id = 0, type = 0;
  while (reader.read_field(id, type)) {
    if (type == 0) return true;
    if (id >= 1 && id <= 3) {
      int64_t value = 0;
      if (!reader.read_int(value)) return false;
      if (id == 1) page.page_type_ = static_cast<int>(value);
      else if (id == 2) page.uncompressed_size_ = static_cast<int32_t>(value);
      else page.compressed_size_ = static_cast<int32_t>(value);
    } else if ((id == 5 || id == 7 || id == 8) && type == 12) {
      bool dictionary = id == 7;
      page.v2_ = id == 8;
      int child_id = 0, child_type = 0;
      while (reader.read_field(child_id, child_type)) {
        if (child_type == 0) break;
        int64_t value = 0;
        if (page.v2_ && child_id == 7) {
          if (!reader.skip(child_type)) return false;
        } else if ((dictionary && child_id <= 2) || (!dictionary && child_id <= 6)) {
          if (!reader.read_int(value)) return false;
          if (child_id == 1) page.num_values_ = static_cast<int32_t>(value);
          else if (!dictionary && page.v2_ && child_id == 2) page.num_nulls_ = static_cast<int32_t>(value);
          else if (!dictionary && child_id == (page.v2_ ? 4 : 2)) page.encoding_ = static_cast<int32_t>(value);
          else if (!dictionary && page.v2_ && child_id == 5) page.definition_bytes_ = static_cast<int32_t>(value);
          else if (!dictionary && page.v2_ && child_id == 6) page.repetition_bytes_ = static_cast<int32_t>(value);
        } else if (!reader.skip(child_type)) {
          return false;
        }
      }
      if (child_type != 0) return false;
    } else if (!reader.skip(type)) {
      return false;
    }
  }
  return false;
}

class RleDecoder
{
public:
  RleDecoder(const char *data, size_t size, int width) : data_(data), size_(size), width_(width) {}
  bool next(uint32_t &value)
  {
    if (width_ == 0) { value = 0; return true; }
    if (run_remaining_ == 0 && !start_run()) return false;
    if (bitpacked_) {
      value = packed_values_[packed_values_.size() - run_remaining_];
    } else {
      value = repeated_value_;
    }
    --run_remaining_;
    return true;
  }
private:
  bool start_run()
  {
    if (pos_ >= size_) return false;
    uint64_t header = 0;
    int shift = 0;
    while (true) {
      if (pos_ >= size_ || shift >= 64) return false;
      uint8_t byte = static_cast<uint8_t>(data_[pos_++]);
      header |= static_cast<uint64_t>(byte & 0x7f) << shift;
      if ((byte & 0x80) == 0) break;
      shift += 7;
    }
    bitpacked_ = (header & 1) != 0;
    if (!bitpacked_) {
      run_remaining_ = header >> 1;
      size_t bytes = static_cast<size_t>((width_ + 7) / 8);
      if (run_remaining_ == 0 || pos_ + bytes > size_) return false;
      repeated_value_ = 0;
      for (size_t i = 0; i < bytes; ++i) repeated_value_ |= static_cast<uint32_t>(static_cast<uint8_t>(data_[pos_++])) << (8 * i);
      return true;
    }
    uint64_t groups = header >> 1;
    if (groups == 0) return false;
    size_t bytes = static_cast<size_t>(groups * width_);
    if (pos_ + bytes > size_) return false;
    size_t total_values = static_cast<size_t>(groups * 8);
    packed_values_.assign(total_values, 0);
    for (size_t i = 0; i < total_values; ++i) {
      size_t bit = i * width_;
      for (int b = 0; b < width_; ++b) {
        if ((static_cast<uint8_t>(data_[pos_ + (bit + b) / 8]) >> ((bit + b) % 8)) & 1) packed_values_[i] |= 1U << b;
      }
    }
    pos_ += bytes;
    run_remaining_ = total_values;
    return true;
  }
  const char *data_;
  size_t size_;
  int width_;
  size_t pos_ = 0;
  bool bitpacked_ = false;
  uint64_t run_remaining_ = 0;
  uint32_t repeated_value_ = 0;
  std::vector<uint32_t> packed_values_;
};

ColumnType parquet_column_type(const ParquetSchemaElement &schema)
{
  if (schema.converted_type_ == 6) return ColumnType::DATE;
  if (schema.converted_type_ == 9 || schema.converted_type_ == 10 || schema.physical_type_ == 3) return ColumnType::DATETIME;
  switch (schema.physical_type_) {
    case 0: return ColumnType::BOOLEAN;
    case 1: case 2: return ColumnType::BIGINT;
    case 4: case 5: return ColumnType::DOUBLE;
    default: return ColumnType::VARCHAR;
  }
}

bool decode_plain(const char *data, size_t size, int physical_type, int converted_type,
                  int64_t count, std::vector<Cell> &values)
{
  size_t pos = 0;
  for (int64_t i = 0; i < count; ++i) {
    if (physical_type == 0) {
      if (pos * 8 >= size * 8) return false;
      values.push_back(Cell::boolean((static_cast<uint8_t>(data[(pos) / 8]) >> (pos % 8)) & 1));
      ++pos;
    } else if (physical_type == 1 || physical_type == 2) {
      size_t bytes = physical_type == 1 ? 4 : 8;
      if (pos + bytes > size) return false;
      int64_t value = physical_type == 1 ? static_cast<int64_t>(static_cast<int32_t>(read_u32(data + pos))) : static_cast<int64_t>(read_u64(data + pos));
      pos += bytes;
      if (converted_type == 6) values.push_back(Cell::temporal(ColumnType::DATE, value));
      else if (converted_type == 9) values.push_back(Cell::temporal(ColumnType::DATETIME, value * 1000));
      else if (converted_type == 10) values.push_back(Cell::temporal(ColumnType::DATETIME, value));
      else values.push_back(Cell::integer(value));
    } else if (physical_type == 3) {
      if (pos + 12 > size) return false;
      uint64_t nanos = read_u64(data + pos);
      uint32_t julian = read_u32(data + pos + 8);
      pos += 12;
      int64_t micros = (static_cast<int64_t>(julian) - 2440588) * 86400000000LL + static_cast<int64_t>(nanos / 1000);
      values.push_back(Cell::temporal(ColumnType::DATETIME, micros));
    } else if (physical_type == 4 || physical_type == 5) {
      size_t bytes = physical_type == 4 ? 4 : 8;
      if (pos + bytes > size) return false;
      double value = 0;
      if (physical_type == 4) {
        uint32_t bits = read_u32(data + pos);
        float float_value = 0;
        std::memcpy(&float_value, &bits, sizeof(bits));
        value = float_value;
      } else {
        uint64_t bits = read_u64(data + pos);
        std::memcpy(&value, &bits, sizeof(bits));
      }
      pos += bytes;
      values.push_back(Cell::real(value));
    } else if (physical_type == 6) {
      if (pos + 4 > size) return false;
      uint32_t len = read_u32(data + pos);
      pos += 4;
      if (len > size - pos) return false;
      values.push_back(Cell::text(std::string(data + pos, len)));
      pos += len;
    } else {
      return false;
    }
  }
  return true;
}

bool snappy_decompress(const char *data, size_t size, std::string &output)
{
  size_t pos = 0;
  uint64_t expected_size = 0;
  int shift = 0;
  while (true) {
    if (pos >= size || shift >= 64) return false;
    uint8_t byte = static_cast<uint8_t>(data[pos++]);
    expected_size |= static_cast<uint64_t>(byte & 0x7f) << shift;
    if ((byte & 0x80) == 0) break;
    shift += 7;
  }
  if (expected_size > std::numeric_limits<size_t>::max()) return false;
  output.clear();
  output.reserve(static_cast<size_t>(expected_size));
  while (pos < size && output.size() < expected_size) {
    uint8_t tag = static_cast<uint8_t>(data[pos++]);
    size_t length = 0;
    size_t offset = 0;
    if ((tag & 3) == 0) {
      length = tag >> 2;
      if (length < 60) {
        ++length;
      } else {
        size_t length_bytes = length - 59;
        if (length_bytes > 4 || pos + length_bytes > size) return false;
        length = 0;
        for (size_t i = 0; i < length_bytes; ++i) length |= static_cast<size_t>(static_cast<uint8_t>(data[pos++])) << (8 * i);
        ++length;
      }
      if (length > size - pos || length > expected_size - output.size()) return false;
      output.append(data + pos, length);
      pos += length;
      continue;
    }
    if ((tag & 3) == 1) {
      length = 4 + ((tag >> 2) & 7);
      if (pos >= size) return false;
      offset = (static_cast<size_t>(tag & 0xe0) << 3) | static_cast<uint8_t>(data[pos++]);
    } else if ((tag & 3) == 2) {
      length = 1 + (tag >> 2);
      if (pos + 2 > size) return false;
      offset = static_cast<uint8_t>(data[pos]) | (static_cast<size_t>(static_cast<uint8_t>(data[pos + 1])) << 8);
      pos += 2;
    } else {
      length = 1 + (tag >> 2);
      if (pos + 4 > size) return false;
      offset = read_u32(data + pos);
      pos += 4;
    }
    if (offset == 0 || offset > output.size() || length > expected_size - output.size()) return false;
    for (size_t i = 0; i < length; ++i) output.push_back(output[output.size() - offset]);
  }
  return pos == size && output.size() == expected_size;
}

int read_parquet_chunk(const std::string &data, const ParquetColumnChunk &chunk,
                       const ParquetSchemaElement &schema, int64_t expected_rows,
                       std::vector<Cell> &output, Error &error)
{
  if (chunk.codec_ != 0 && chunk.codec_ != 1 && chunk.codec_ != 6) {
    return fail(error, ErrorCode::NOT_SUPPORTED, "unsupported Parquet compression codec: " + std::to_string(chunk.codec_));
  }
  if (chunk.physical_type_ != schema.physical_type_) return fail(error, ErrorCode::DATA_ERROR, "Parquet column type mismatch");
  size_t pos = static_cast<size_t>(chunk.dictionary_page_offset_ > 0 ? chunk.dictionary_page_offset_ : chunk.data_page_offset_);
  if (pos >= data.size()) return fail(error, ErrorCode::DATA_ERROR, "invalid Parquet page offset");
  std::vector<Cell> dictionary;
  while (static_cast<int64_t>(output.size()) < expected_rows && pos < data.size()) {
    CompactReader reader(data, pos);
    ParquetPageHeader page;
    if (!parse_page_header(reader, page) || page.compressed_size_ < 0 || page.uncompressed_size_ < 0) {
      return fail(error, ErrorCode::DATA_ERROR, "invalid Parquet page header");
    }
    pos = reader.position();
    if (static_cast<size_t>(page.compressed_size_) > data.size() - pos) {
      return fail(error, ErrorCode::DATA_ERROR, "truncated Parquet page");
    }
    size_t compressed_size = static_cast<size_t>(page.compressed_size_);
    size_t level_size = page.v2_ ? static_cast<size_t>(page.definition_bytes_ + page.repetition_bytes_) : 0;
    if (level_size > compressed_size || level_size > static_cast<size_t>(page.uncompressed_size_)) {
      return fail(error, ErrorCode::DATA_ERROR, "invalid Parquet V2 level byte counts");
    }
    std::string decompressed;
    std::string page_storage;
    const char *page_data = data.data() + pos;
    size_t page_size = compressed_size;
    if (chunk.codec_ != 0) {
      const char *compressed_data = page_data + level_size;
      size_t compressed_data_size = compressed_size - level_size;
      size_t uncompressed_data_size = static_cast<size_t>(page.uncompressed_size_) - level_size;
      if (chunk.codec_ == 1) {
        if (!snappy_decompress(compressed_data, compressed_data_size, decompressed) ||
            decompressed.size() != uncompressed_data_size) {
          return fail(error, ErrorCode::DATA_ERROR, "invalid Snappy-compressed Parquet page");
        }
      } else {
        if (uncompressed_data_size == 0) {
          if (compressed_data_size != 0) {
            return fail(error, ErrorCode::DATA_ERROR, "invalid Zstd-compressed Parquet page");
          }
        } else {
          decompressed.resize(uncompressed_data_size);
          oceanbase::common::zstd_1_3_8::OB_ZSTD_customMem zstd_mem{};
          size_t decompressed_size = 0;
          int zstd_ret = oceanbase::common::zstd_1_3_8::ObZstdWrapper::decompress(
              zstd_mem, compressed_data, compressed_data_size, decompressed.data(), decompressed.size(), decompressed_size);
          if (zstd_ret != 0 || decompressed_size != decompressed.size()) {
            return fail(error, ErrorCode::DATA_ERROR, "invalid Zstd-compressed Parquet page");
          }
        }
      }
      page_storage.assign(page_data, level_size);
      page_storage.append(decompressed);
      page_data = page_storage.data();
      page_size = page_storage.size();
    }
    if (page.page_type_ == 2) {
      dictionary.clear();
      if (!decode_plain(page_data, page_size, chunk.physical_type_, schema.converted_type_, page.num_values_, dictionary)) {
        return fail(error, ErrorCode::DATA_ERROR, "invalid Parquet dictionary page");
      }
    } else if (page.page_type_ == 0 || page.page_type_ == 3) {
      if (page.num_values_ < 0 || page.num_nulls_ < 0) return fail(error, ErrorCode::DATA_ERROR, "invalid Parquet value count");
      size_t data_pos = 0;
      std::vector<uint32_t> definitions;
      bool optional = schema.repetition_type_ == 1;
      int64_t nonnull_count = page.num_values_;
      if (optional) {
        const char *definition_data = page_data;
        size_t definition_size = 0;
        if (page.v2_) {
          definition_size = static_cast<size_t>(page.definition_bytes_);
          if (definition_size > page_size) return fail(error, ErrorCode::DATA_ERROR, "invalid Parquet definition levels");
          data_pos = definition_size + static_cast<size_t>(page.repetition_bytes_);
        } else {
          if (page_size < 4) return fail(error, ErrorCode::DATA_ERROR, "missing Parquet definition levels");
          definition_size = read_u32(page_data);
          definition_data += 4;
          data_pos = 4 + definition_size;
        }
        if (data_pos > page_size) return fail(error, ErrorCode::DATA_ERROR, "invalid Parquet level data");
        RleDecoder decoder(definition_data, definition_size, 1);
        nonnull_count = 0;
        for (int32_t i = 0; i < page.num_values_; ++i) {
          uint32_t level = 0;
          if (!decoder.next(level) || level > 1) return fail(error, ErrorCode::DATA_ERROR, "invalid Parquet definition level");
          definitions.push_back(level);
          if (level == 1) ++nonnull_count;
        }
      }
      if (page.v2_ && nonnull_count != page.num_values_ - page.num_nulls_) {
        return fail(error, ErrorCode::DATA_ERROR, "inconsistent Parquet null count");
      }
      std::vector<Cell> values;
      if (page.encoding_ == 0) {
        if (!decode_plain(page_data + data_pos, page_size - data_pos, chunk.physical_type_, schema.converted_type_, nonnull_count, values)) {
          return fail(error, ErrorCode::DATA_ERROR, "invalid PLAIN Parquet data page");
        }
      } else if (page.encoding_ == 2 || page.encoding_ == 8) {
        if (data_pos >= page_size) return fail(error, ErrorCode::DATA_ERROR, "missing Parquet dictionary indices");
        int width = static_cast<uint8_t>(page_data[data_pos++]);
        if (width < 0 || width > 32) return fail(error, ErrorCode::DATA_ERROR, "invalid Parquet dictionary bit width");
        RleDecoder decoder(page_data + data_pos, page_size - data_pos, width);
        for (int64_t i = 0; i < nonnull_count; ++i) {
          uint32_t index = 0;
          if (!decoder.next(index) || index >= dictionary.size()) return fail(error, ErrorCode::DATA_ERROR, "invalid Parquet dictionary index");
          values.push_back(dictionary[index]);
        }
      } else {
        return fail(error, ErrorCode::NOT_SUPPORTED, "unsupported Parquet page encoding: " + std::to_string(page.encoding_));
      }
      size_t value_idx = 0;
      for (int32_t i = 0; i < page.num_values_; ++i) {
        if (optional && definitions[i] == 0) output.push_back(Cell::null());
        else output.push_back(values[value_idx++]);
      }
    }
    pos += compressed_size;
  }
  if (static_cast<int64_t>(output.size()) != expected_rows) {
    return fail(error, ErrorCode::DATA_ERROR, "Parquet row group is truncated");
  }
  return 0;
}

int read_parquet(const Request &request, Table &table, Error &error)
{
  if (!request.format_.empty() && !iequals(request.format_, "parquet")) {
    return fail(error, ErrorCode::INVALID_ARGUMENT, "unsupported Parquet format: " + request.format_);
  }
  std::string data;
  if (read_file(request.path_, data, error) != 0) return -1;
  if (data.size() < 12 || data.compare(0, 4, "PAR1") != 0 || data.compare(data.size() - 4, 4, "PAR1") != 0) {
    return fail(error, ErrorCode::DATA_ERROR, "invalid Parquet file magic");
  }
  uint32_t footer_size = read_u32(data.data() + data.size() - 8);
  if (footer_size > data.size() - 12) return fail(error, ErrorCode::DATA_ERROR, "invalid Parquet footer size");
  ParquetMetadata metadata;
  if (!parse_metadata(data, data.size() - 8 - footer_size, metadata)) {
    return fail(error, ErrorCode::DATA_ERROR, "invalid Parquet metadata");
  }
  if (metadata.schema_.size() < 2 || metadata.num_rows_ < 0) {
    return fail(error, ErrorCode::DATA_ERROR, "Parquet file has no flat columns");
  }
  for (size_t i = 1; i < metadata.schema_.size(); ++i) {
    Column column;
    column.name_ = metadata.schema_[i].name_;
    column.type_ = parquet_column_type(metadata.schema_[i]);
    table.columns_.push_back(column);
  }
  std::vector<std::vector<Cell>> column_values(table.columns_.size());
  int64_t row_offset = 0;
  for (const ParquetRowGroup &group : metadata.row_groups_) {
    if (group.num_rows_ < 0 || group.columns_.size() != table.columns_.size()) {
      return fail(error, ErrorCode::DATA_ERROR, "unsupported nested or inconsistent Parquet schema");
    }
    for (size_t col = 0; col < group.columns_.size(); ++col) {
      std::vector<Cell> values;
      if (read_parquet_chunk(data, group.columns_[col], metadata.schema_[col + 1], group.num_rows_, values, error) != 0) return -1;
      column_values[col].insert(column_values[col].end(), values.begin(), values.end());
    }
    row_offset += group.num_rows_;
  }
  if (row_offset != metadata.num_rows_) {
    return fail(error, ErrorCode::DATA_ERROR, "inconsistent Parquet row count: groups=" +
                std::to_string(row_offset) + ", footer=" + std::to_string(metadata.num_rows_));
  }
  table.rows_.resize(static_cast<size_t>(metadata.num_rows_));
  for (size_t col = 0; col < column_values.size(); ++col) {
    if (column_values[col].size() != table.rows_.size()) return fail(error, ErrorCode::DATA_ERROR, "inconsistent Parquet column length");
    for (size_t row = 0; row < table.rows_.size(); ++row) {
      table.rows_[row].resize(table.columns_.size());
      table.rows_[row][col] = column_values[col][row];
      if (table.rows_[row][col].is_null_) table.columns_[col].nullable_ = true;
    }
  }
  return 0;
}

} // namespace

Cell Cell::null() { return Cell(); }
Cell Cell::integer(int64_t value) { Cell cell; cell.is_null_ = false; cell.int_value_ = value; return cell; }
Cell Cell::real(double value) { Cell cell; cell.is_null_ = false; cell.double_value_ = value; return cell; }
Cell Cell::boolean(bool value) { Cell cell; cell.is_null_ = false; cell.int_value_ = value ? 1 : 0; return cell; }
Cell Cell::text(const std::string &value) { Cell cell; cell.is_null_ = false; cell.string_value_ = value; return cell; }
Cell Cell::temporal(ColumnType, int64_t value) { Cell cell; cell.is_null_ = false; cell.int_value_ = value; return cell; }

const char *column_type_name(ColumnType type)
{
  switch (type) {
    case ColumnType::BIGINT: return "BIGINT";
    case ColumnType::DOUBLE: return "DOUBLE";
    case ColumnType::VARCHAR: return "VARCHAR";
    case ColumnType::BOOLEAN: return "BOOLEAN";
    case ColumnType::DATE: return "DATE";
    case ColumnType::DATETIME: return "DATETIME";
  }
  return "VARCHAR";
}

std::string Request::serialize() const
{
  std::string out("FSQ1", 4);
  out.push_back(static_cast<char>(kind_));
  out.push_back(header_ ? 1 : 0);
  append_u64(out, static_cast<uint64_t>(sample_size_));
  append_string(out, format_);
  append_string(out, path_);
  return out;
}

bool Request::deserialize(const std::string &input)
{
  if (input.size() < 14 || input.compare(0, 4, "FSQ1") != 0) return false;
  kind_ = static_cast<FunctionKind>(static_cast<uint8_t>(input[4]));
  if (static_cast<uint8_t>(kind_) > static_cast<uint8_t>(FunctionKind::GLOB)) return false;
  header_ = input[5] != 0;
  sample_size_ = static_cast<int64_t>(read_u64(input.data() + 6));
  size_t pos = 14;
  return take_string(input, pos, format_) && take_string(input, pos, path_) && pos == input.size();
}

std::string csv_escape(const std::string &value)
{
  if (value.find_first_of(",\"\r\n") == std::string::npos) return value;
  std::string out("\"");
  for (char c : value) {
    if (c == '"') out.push_back('"');
    out.push_back(c);
  }
  out.push_back('"');
  return out;
}

int read_table(const Request &request, Table &table, Error &error)
{
  error.reset();
  table.clear();
  switch (request.kind_) {
    case FunctionKind::CSV: return read_csv(request, table, error);
    case FunctionKind::JSON: return read_json(request, table, error);
    case FunctionKind::PARQUET: return read_parquet(request, table, error);
    case FunctionKind::GLOB: return read_glob(request, table, error);
  }
  return fail(error, ErrorCode::INVALID_ARGUMENT, "unknown file table function");
}

} // namespace file_sql
} // namespace sql
} // namespace oceanbase
