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

#pragma once

#include <cstdint>
#include <string>
#include <vector>

namespace oceanbase
{
namespace sql
{
namespace file_sql
{

enum class FunctionKind : uint8_t
{
  CSV = 0,
  JSON = 1,
  PARQUET = 2,
  GLOB = 3,
};

enum class ColumnType : uint8_t
{
  BIGINT = 0,
  DOUBLE = 1,
  VARCHAR = 2,
  BOOLEAN = 3,
  DATE = 4,
  DATETIME = 5,
};

enum class ErrorCode
{
  SUCCESS = 0,
  INVALID_ARGUMENT,
  NOT_SUPPORTED,
  IO_ERROR,
  DATA_ERROR,
};

struct Error
{
  ErrorCode code = ErrorCode::SUCCESS;
  std::string message_;
  void reset() { code = ErrorCode::SUCCESS; message_.clear(); }
};

struct Request
{
  FunctionKind kind_ = FunctionKind::CSV;
  std::string path_;
  std::string format_;
  bool header_ = true;
  int64_t sample_size_ = 20480;

  std::string serialize() const;
  bool deserialize(const std::string &input);
};

struct Cell
{
  bool is_null_ = true;
  int64_t int_value_ = 0;
  double double_value_ = 0.0;
  std::string string_value_;

  static Cell null();
  static Cell integer(int64_t value);
  static Cell real(double value);
  static Cell boolean(bool value);
  static Cell text(const std::string &value);
  static Cell temporal(ColumnType type, int64_t value);
};

struct Column
{
  std::string name_;
  ColumnType type_ = ColumnType::VARCHAR;
  bool nullable_ = false;
};

struct Table
{
  std::vector<Column> columns_;
  std::vector<std::vector<Cell>> rows_;
  void clear() { columns_.clear(); rows_.clear(); }
};

const char *column_type_name(ColumnType type);
int read_table(const Request &request, Table &table, Error &error);
std::string csv_escape(const std::string &value);

} // namespace file_sql
} // namespace sql
} // namespace oceanbase
