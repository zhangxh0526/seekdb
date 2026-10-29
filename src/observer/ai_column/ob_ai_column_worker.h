/*
 * Copyright (c) 2025 OceanBase.
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

#include "lib/task/ob_timer.h"
#include "common/mysqlclient/ob_mysql_proxy.h"

namespace oceanbase
{
namespace observer
{

class ObAIColumnWorker : public common::ObTimerTask
{
public:
  ObAIColumnWorker();
  virtual ~ObAIColumnWorker();
  int init(common::ObMySQLProxy &sql_proxy);
  int start();
  void stop();
  void wait();
  void destroy();
  virtual void runTimerTask() override;
  int do_work();
private:
  int process_next_group();
  common::ObMySQLProxy *sql_proxy_;
  common::ObTimer timer_;
  bool is_inited_;
  bool is_paused_;
};

} // namespace observer
} // namespace oceanbase
