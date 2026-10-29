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

#define USING_LOG_PREFIX SERVER
#include "observer/ai_column/ob_ai_column_worker.h"
#include "lib/time/ob_time_utility.h"
#include "query/ai/ob_ai_endpoint_resolver.h"
#include "share/ob_server_struct.h"
#include "share/rc/ob_server_runtime.h"
#include "sql/ai_column/ob_ai_column_util.h"
#include "sql/engine/expr/ob_expr_ai/ob_ai_func_client.h"
#include "sql/engine/expr/ob_expr_ai/ob_ai_func_utils.h"

namespace oceanbase
{
namespace observer
{
using namespace common;
using namespace share;
using namespace share::schema;
using namespace sql;

namespace
{
constexpr int64_t AI_WORKER_PERIOD_US = 50 * 1000;

struct AITask
{
  AITask() : table_id_(0), column_id_(0), retry_count_(0), max_retries_(0) {}
  uint64_t table_id_;
  uint64_t column_id_;
  int64_t retry_count_;
  int64_t max_retries_;
  ObString rowkey_predicate_;
  ObString request_id_;
  ObString function_type_;
  ObString model_name_;
  ObString prompt_expr_;
  ObString system_prompt_;
  ObString database_name_;
  ObString table_name_;
  ObString column_name_;
  ObString state_column_name_;
  ObString request_column_name_;
  ObString prompt_;
  ObString result_;
};

int append_identifier(ObSqlString &sql, const ObString &name)
{
  int ret = OB_SUCCESS;
  if (OB_FAIL(sql.append("`"))) {
  } else {
    for (int64_t i = 0; OB_SUCC(ret) && i < name.length(); ++i) {
      if (name.ptr()[i] == '`') {
        ret = sql.append("``");
      } else {
        ret = sql.append(name.ptr() + i, 1);
      }
    }
  }
  if (OB_SUCC(ret)) {
    ret = sql.append("`");
  }
  return ret;
}

int append_literal(ObSqlString &sql, const ObString &value)
{
  int ret = OB_SUCCESS;
  if (OB_FAIL(sql.append("'"))) {
  } else {
    for (int64_t i = 0; OB_SUCC(ret) && i < value.length(); ++i) {
      if (value.ptr()[i] == '\'') {
        ret = sql.append("''");
      } else if (value.ptr()[i] == '\\') {
        ret = sql.append("\\\\");
      } else {
        ret = sql.append(value.ptr() + i, 1);
      }
    }
  }
  if (OB_SUCC(ret)) {
    ret = sql.append("'");
  }
  return ret;
}

int append_qualified_table(ObSqlString &sql, const AITask &task)
{
  int ret = OB_SUCCESS;
  if (OB_FAIL(append_identifier(sql, task.database_name_))) {
  } else if (OB_FAIL(sql.append("."))) {
  } else if (OB_FAIL(append_identifier(sql, task.table_name_))) {
  }
  return ret;
}

int append_group_predicate(ObSqlString &sql, const AITask &task, bool include_request)
{
  int ret = OB_SUCCESS;
  if (OB_FAIL(sql.append_fmt("table_id=%lu AND rowkey_predicate=", task.table_id_))) {
  } else if (OB_FAIL(append_literal(sql, task.rowkey_predicate_))) {
  } else if (include_request && OB_FAIL(sql.append(" AND request_id="))) {
  }
  if (OB_FAIL(ret) || !include_request) {
  } else if (OB_FAIL(append_literal(sql, task.request_id_))) {
  }
  return ret;
}

int deep_copy_string(ObIAllocator &allocator, const ObString &src, ObString &dst)
{
  return ob_write_string(allocator, src, dst);
}

int read_tasks(ObMySQLProxy &proxy, ObIAllocator &allocator,
               ObSEArray<AITask, 8> &tasks, bool &found)
{
  int ret = OB_SUCCESS;
  found = false;
  uint64_t table_id = 0;
  int64_t signed_table_id = 0;
  ObString rowkey_predicate;
  ObString request_id;
  {
    ObSqlString sql;
    sql.assign("SELECT task.table_id,task.rowkey_predicate,task.request_id FROM oceanbase.__all_ai_column_task task "
        "JOIN oceanbase.__all_table t ON t.table_id=task.table_id "
        "JOIN oceanbase.__all_ai_column a ON a.table_id=task.table_id AND a.column_id=task.column_id "
        "WHERE task.state IN (1,3) AND task.eligible_ts<=NOW(6) "
        "ORDER BY task.table_id,task.rowkey_predicate,task.request_id,task.column_id LIMIT 1");
    SMART_VAR(ObMySQLProxy::MySQLResult, res) {
      if (OB_FAIL(proxy.read(res, sql.ptr()))) {
      } else if (OB_ISNULL(res.get_result())) {
        ret = OB_ERR_UNEXPECTED;
      } else {
        int next_ret = res.get_result()->next();
        if (OB_ITER_END == next_ret) {
        } else if (OB_FAIL(next_ret)) {
          ret = next_ret;
        } else if (OB_FAIL(res.get_result()->get_int(static_cast<int64_t>(0), signed_table_id))
                   || OB_FAIL(res.get_result()->get_varchar(static_cast<int64_t>(1), rowkey_predicate))
                   || OB_FAIL(res.get_result()->get_varchar(static_cast<int64_t>(2), request_id))) {
        }
        int close_ret = res.get_result()->close();
        if (OB_SUCCESS == ret) {
          ret = close_ret;
        }
      }
    }
  }
  table_id = static_cast<uint64_t>(signed_table_id);
  if (OB_FAIL(ret) || rowkey_predicate.empty() || request_id.empty()) {
    return ret;
  }
  ObSqlString sql;
  if (OB_FAIL(sql.assign("SELECT task.table_id,task.column_id,task.rowkey_predicate,task.request_id,"
      "task.retry_count,a.max_retries,a.function_type,a.model_name,a.prompt_expr,a.system_prompt,"
      "d.database_name,t.table_name,a.column_name,state_col.column_name,request_col.column_name "
      "FROM oceanbase.__all_ai_column_task task "
      "JOIN oceanbase.__all_ai_column a ON a.table_id=task.table_id AND a.column_id=task.column_id "
      "JOIN oceanbase.__all_table t ON t.table_id=task.table_id "
      "JOIN oceanbase.__all_database d ON d.database_id=t.database_id "
      "JOIN oceanbase.__all_column state_col ON state_col.table_id=a.table_id AND state_col.column_id=a.state_column_id "
      "JOIN oceanbase.__all_column request_col ON request_col.table_id=a.table_id AND request_col.column_id=a.request_column_id "))) {
  } else if (OB_FAIL(sql.append_fmt("WHERE task.table_id=%lu AND task.rowkey_predicate=", table_id))) {
  } else if (OB_FAIL(append_literal(sql, rowkey_predicate))) {
  } else if (OB_FAIL(sql.append(" AND task.request_id="))) {
  } else if (OB_FAIL(append_literal(sql, request_id))) {
  } else if (OB_FAIL(sql.append(" AND task.state IN (1,3) AND task.eligible_ts<=NOW(6) "
                                "ORDER BY task.column_id"))) {
  }
  SMART_VAR(ObMySQLProxy::MySQLResult, res) {
    if (OB_FAIL(ret)) {
    } else if (OB_FAIL(proxy.read(res, sql.ptr()))) {
    } else if (OB_ISNULL(res.get_result())) {
      ret = OB_ERR_UNEXPECTED;
    } else {
      ObSEArray<AITask, 8> candidates;
      while (OB_SUCC(ret) && OB_ITER_END != (ret = res.get_result()->next())) {
        AITask task;
        ObString value;
        int64_t signed_id = 0;
        if (OB_FAIL(res.get_result()->get_int(static_cast<int64_t>(0), signed_id))) {
          continue;
        }
        task.table_id_ = static_cast<uint64_t>(signed_id);
        if (OB_FAIL(res.get_result()->get_int(static_cast<int64_t>(1), signed_id))) {
          continue;
        }
        task.column_id_ = static_cast<uint64_t>(signed_id);
        if (OB_FAIL(res.get_result()->get_varchar(static_cast<int64_t>(2), value)) || OB_FAIL(deep_copy_string(allocator, value, task.rowkey_predicate_))
            || OB_FAIL(res.get_result()->get_varchar(static_cast<int64_t>(3), value)) || OB_FAIL(deep_copy_string(allocator, value, task.request_id_))
            || OB_FAIL(res.get_result()->get_int(static_cast<int64_t>(4), task.retry_count_))
            || OB_FAIL(res.get_result()->get_int(static_cast<int64_t>(5), task.max_retries_))
            || OB_FAIL(res.get_result()->get_varchar(static_cast<int64_t>(6), value)) || OB_FAIL(deep_copy_string(allocator, value, task.function_type_))
            || OB_FAIL(res.get_result()->get_varchar(static_cast<int64_t>(7), value)) || OB_FAIL(deep_copy_string(allocator, value, task.model_name_))
            || OB_FAIL(res.get_result()->get_varchar(static_cast<int64_t>(8), value)) || OB_FAIL(deep_copy_string(allocator, value, task.prompt_expr_))
            || OB_FAIL(res.get_result()->get_varchar(static_cast<int64_t>(9), value)) || OB_FAIL(deep_copy_string(allocator, value, task.system_prompt_))
            || OB_FAIL(res.get_result()->get_varchar(static_cast<int64_t>(10), value)) || OB_FAIL(deep_copy_string(allocator, value, task.database_name_))
            || OB_FAIL(res.get_result()->get_varchar(static_cast<int64_t>(11), value)) || OB_FAIL(deep_copy_string(allocator, value, task.table_name_))
            || OB_FAIL(res.get_result()->get_varchar(static_cast<int64_t>(12), value)) || OB_FAIL(deep_copy_string(allocator, value, task.column_name_))
            || OB_FAIL(res.get_result()->get_varchar(static_cast<int64_t>(13), value)) || OB_FAIL(deep_copy_string(allocator, value, task.state_column_name_))
            || OB_FAIL(res.get_result()->get_varchar(static_cast<int64_t>(14), value)) || OB_FAIL(deep_copy_string(allocator, value, task.request_column_name_))
            || OB_FAIL(candidates.push_back(task))) {
          break;
        }
      }
      if (ret == OB_ITER_END) {
        ret = OB_SUCCESS;
      }
      if (OB_SUCC(ret) && !candidates.empty()) {
        const AITask &first = candidates.at(0);
        for (int64_t i = 0; OB_SUCC(ret) && i < candidates.count(); ++i) {
          const AITask &task = candidates.at(i);
          if (task.table_id_ == first.table_id_
              && task.rowkey_predicate_ == first.rowkey_predicate_
              && task.request_id_ == first.request_id_) {
            if (OB_FAIL(tasks.push_back(task))) {
            }
          }
        }
        found = !tasks.empty();
      }
      int close_ret = res.get_result()->close();
      if (OB_SUCCESS == ret) {
        ret = close_ret;
      }
    }
  }
  return ret;
}

int call_model(ObIAllocator &allocator, AITask &task)
{
  int ret = OB_SUCCESS;
  ObAIFuncExprInfo *info = nullptr;
  ObAiModelEndpointInfo endpoint;
  query::ObIAiEndpointResolver *resolver =
      share::server_service<query::ObIAiEndpointResolver>();
  if (OB_FAIL(ObAIFuncUtils::get_ai_func_info(allocator, task.model_name_, info))) {
  } else if (OB_ISNULL(info) || OB_ISNULL(resolver)) {
    ret = OB_ERR_UNEXPECTED;
  } else if (OB_FAIL(resolver->resolve_by_model_name(task.model_name_, allocator, endpoint))) {
  } else if (task.function_type_ == "embed") {
    ObAIFuncModel model(allocator, *info, endpoint);
    ret = model.call_dense_embedding(task.prompt_, nullptr, task.result_);
  } else {
    ObAIFuncIComplete *provider = nullptr;
    ObAIFuncClient client;
    client.set_max_retry_times(0);
    ObArray<ObString> headers;
    ObJsonObject *body = nullptr;
    ObJsonObject *response = nullptr;
    ObIJsonBase *result_base = nullptr;
    ObString access_key;
    ObString request_model = endpoint.get_request_model_name().empty() ? info->model_ : endpoint.get_request_model_name();
    if (OB_FAIL(ObAIFuncUtils::get_complete_provider(allocator, endpoint.get_provider(), provider))) {
    } else if (OB_ISNULL(provider)) {
      ret = OB_ERR_UNEXPECTED;
    } else if (OB_FAIL(endpoint.get_unencrypted_access_key(allocator, access_key))) {
    } else if (OB_FAIL(provider->get_header(allocator, access_key, headers))) {
    } else if (OB_FAIL(provider->get_body(allocator, request_model, task.system_prompt_, task.prompt_, nullptr, body))) {
    } else if (OB_FAIL(client.send_post(allocator, endpoint.get_url(), headers, body, response))) {
    } else if (OB_FAIL(provider->parse_output(allocator, response, result_base))) {
    } else if (OB_FAIL(ObAIFuncJsonUtils::print_json_to_str(allocator, result_base, task.result_))) {
    }
  }
  return ret;
}

int evaluate_prompt(ObMySQLProxy &proxy, ObIAllocator &allocator, AITask &task)
{
  int ret = OB_SUCCESS;
  ObSqlString sql;
  if (OB_FAIL(sql.append_fmt("SELECT %.*s FROM ", task.prompt_expr_.length(), task.prompt_expr_.ptr()))) {
  } else if (OB_FAIL(append_qualified_table(sql, task))) {
  } else if (OB_FAIL(sql.append_fmt(" WHERE %.*s AND ", task.rowkey_predicate_.length(), task.rowkey_predicate_.ptr()))) {
  } else if (OB_FAIL(append_identifier(sql, task.request_column_name_))) {
  } else if (OB_FAIL(sql.append("="))) {
  } else if (OB_FAIL(append_literal(sql, task.request_id_))) {
  }
  SMART_VAR(ObMySQLProxy::MySQLResult, res) {
    if (OB_FAIL(ret)) {
    } else if (OB_FAIL(proxy.read(res, sql.ptr()))) {
    } else if (OB_ISNULL(res.get_result())) {
      ret = OB_ERR_UNEXPECTED;
    } else if (OB_FAIL(res.get_result()->next())) {
    } else {
      ObString prompt;
      if (OB_FAIL(res.get_result()->get_varchar(static_cast<int64_t>(0), prompt))) {
      } else if (OB_FAIL(deep_copy_string(allocator, prompt, task.prompt_))) {
      }
      int close_ret = res.get_result()->close();
      if (OB_SUCCESS == ret) {
        ret = close_ret;
      }
    }
  }
  return ret;
}

int update_hidden_states(ObMySQLTransaction &trans, ObSEArray<AITask, 8> &tasks,
                         const int64_t state, bool clear_results)
{
  int ret = OB_SUCCESS;
  ObSqlString sql;
  const AITask &first = tasks.at(0);
  if (OB_FAIL(sql.append("UPDATE "))) {
  } else if (OB_FAIL(append_qualified_table(sql, first))) {
  } else if (OB_FAIL(sql.append(" SET "))) {
  }
  for (int64_t i = 0; OB_SUCC(ret) && i < tasks.count(); ++i) {
    AITask &task = tasks.at(i);
    if (i > 0 && OB_FAIL(sql.append(","))) {
    }
    if (OB_FAIL(ret) || !clear_results) {
    } else if (OB_FAIL(append_identifier(sql, task.column_name_))) {
    } else if (OB_FAIL(sql.append("=NULL,"))) {
    }
    if (OB_FAIL(ret)) {
    } else if (OB_FAIL(append_identifier(sql, task.state_column_name_))) {
    } else if (OB_FAIL(sql.append_fmt("=%ld", state))) {
    }
  }
  if (OB_FAIL(ret)) {
  } else if (OB_FAIL(sql.append_fmt(" WHERE %.*s", first.rowkey_predicate_.length(), first.rowkey_predicate_.ptr()))) {
  }
  for (int64_t i = 0; OB_SUCC(ret) && i < tasks.count(); ++i) {
    AITask &task = tasks.at(i);
    if (OB_FAIL(sql.append(" AND "))) {
    } else if (OB_FAIL(append_identifier(sql, task.request_column_name_))) {
    } else if (OB_FAIL(sql.append("="))) {
    } else if (OB_FAIL(append_literal(sql, task.request_id_))) {
    }
  }
  if (OB_SUCC(ret)) {
    int64_t affected_rows = 0;
    ret = trans.write(sql.ptr(), affected_rows);
  }
  return ret;
}

int mark_running(ObMySQLProxy &proxy, ObSEArray<AITask, 8> &tasks)
{
  int ret = OB_SUCCESS;
  ObMySQLTransaction trans;
  ObSqlString sql;
  const AITask &first = tasks.at(0);
  if (OB_FAIL(trans.start(&proxy))) {
  } else if (OB_FAIL(sql.append("UPDATE oceanbase.__all_ai_column_task SET state=2 WHERE "))) {
  } else if (OB_FAIL(append_group_predicate(sql, first, true))) {
  } else {
    int64_t affected_rows = 0;
    if (OB_FAIL(trans.write(sql.ptr(), affected_rows))) {
    } else if (OB_FAIL(update_hidden_states(trans, tasks, AI_COLUMN_RUNNING, false))) {
    }
  }
  if (trans.is_started()) {
    int tmp_ret = trans.end(OB_SUCC(ret));
    if (OB_SUCCESS == ret) {
      ret = tmp_ret;
    }
  }
  return ret;
}

int finish_success(ObMySQLProxy &proxy, ObSEArray<AITask, 8> &tasks)
{
  int ret = OB_SUCCESS;
  ObMySQLTransaction trans;
  ObSqlString sql;
  const AITask &first = tasks.at(0);
  if (OB_FAIL(trans.start(&proxy))) {
  } else if (OB_FAIL(sql.append("UPDATE "))) {
  } else if (OB_FAIL(append_qualified_table(sql, first))) {
  } else if (OB_FAIL(sql.append(" SET "))) {
  }
  for (int64_t i = 0; OB_SUCC(ret) && i < tasks.count(); ++i) {
    AITask &task = tasks.at(i);
    if (i > 0 && OB_FAIL(sql.append(","))) {
    }
    if (OB_FAIL(ret)) {
    } else if (OB_FAIL(append_identifier(sql, task.column_name_))) {
    } else if (OB_FAIL(sql.append("="))) {
    } else if (OB_FAIL(append_literal(sql, task.result_))) {
    } else if (OB_FAIL(sql.append(","))) {
    } else if (OB_FAIL(append_identifier(sql, task.state_column_name_))) {
    } else if (OB_FAIL(sql.append("=0"))) {
    }
  }
  if (OB_FAIL(ret)) {
  } else if (OB_FAIL(sql.append_fmt(" WHERE %.*s", first.rowkey_predicate_.length(), first.rowkey_predicate_.ptr()))) {
  }
  for (int64_t i = 0; OB_SUCC(ret) && i < tasks.count(); ++i) {
    AITask &task = tasks.at(i);
    if (OB_FAIL(sql.append(" AND "))) {
    } else if (OB_FAIL(append_identifier(sql, task.request_column_name_))) {
    } else if (OB_FAIL(sql.append("="))) {
    } else if (OB_FAIL(append_literal(sql, task.request_id_))) {
    }
  }
  if (OB_FAIL(ret)) {
  } else {
    int64_t affected_rows = 0;
    if (OB_FAIL(trans.write(sql.ptr(), affected_rows))) {
    } else if (FALSE_IT(sql.reuse())) {
    } else if (OB_FAIL(sql.append("DELETE FROM oceanbase.__all_ai_column_task WHERE "))) {
    } else if (OB_FAIL(append_group_predicate(sql, first, true))) {
    } else if (OB_FAIL(trans.write(sql.ptr(), affected_rows))) {
    }
  }
  if (trans.is_started()) {
    int tmp_ret = trans.end(OB_SUCC(ret));
    if (OB_SUCCESS == ret) {
      ret = tmp_ret;
    }
  }
  return ret;
}

int finish_failure(ObMySQLProxy &proxy, AITask &failed_task, ObSEArray<AITask, 8> &tasks)
{
  int ret = OB_SUCCESS;
  const bool retry = failed_task.retry_count_ < failed_task.max_retries_;
  const int64_t next_retry = failed_task.retry_count_ + 1;
  ObMySQLTransaction trans;
  ObSqlString sql;
  if (OB_FAIL(trans.start(&proxy))) {
  } else if (retry && OB_FAIL(sql.append_fmt(
                 "UPDATE oceanbase.__all_ai_column_task SET state=3,retry_count=%ld,"
                 "eligible_ts=DATE_ADD(NOW(6),INTERVAL 100000 MICROSECOND) WHERE ", next_retry))) {
  } else if (!retry && OB_FAIL(sql.append("DELETE FROM oceanbase.__all_ai_column_task WHERE "))) {
  } else if (OB_FAIL(append_group_predicate(sql, failed_task, true))) {
  } else {
    int64_t affected_rows = 0;
    if (OB_FAIL(trans.write(sql.ptr(), affected_rows))) {
    } else if (OB_FAIL(update_hidden_states(trans, tasks,
                   retry ? AI_COLUMN_RETRY : AI_COLUMN_FAILED, !retry))) {
    }
  }
  if (trans.is_started()) {
    int tmp_ret = trans.end(OB_SUCC(ret));
    if (OB_SUCCESS == ret) {
      ret = tmp_ret;
    }
  }
  return ret;
}
} // namespace

ObAIColumnWorker::ObAIColumnWorker()
  : sql_proxy_(nullptr), timer_(), is_inited_(false), is_paused_(false)
{
}

ObAIColumnWorker::~ObAIColumnWorker()
{
  destroy();
}

int ObAIColumnWorker::init(ObMySQLProxy &sql_proxy)
{
  int ret = OB_SUCCESS;
  if (is_inited_) {
    ret = OB_INIT_TWICE;
  } else if (OB_FAIL(timer_.init("AIColumn", ObMemAttr("AIColumn")))) {
  } else {
    sql_proxy_ = &sql_proxy;
    disable_timeout_check();
    is_inited_ = true;
  }
  return ret;
}

int ObAIColumnWorker::start()
{
  int ret = OB_SUCCESS;
  if (!is_inited_) {
    ret = OB_NOT_INIT;
  } else if (OB_FAIL(timer_.schedule(*this, AI_WORKER_PERIOD_US, true))) {
  }
  return ret;
}

void ObAIColumnWorker::stop()
{
  if (timer_.inited()) {
    timer_.stop();
  }
}

void ObAIColumnWorker::wait()
{
  if (timer_.inited()) {
    timer_.wait();
  }
}

void ObAIColumnWorker::destroy()
{
  if (timer_.inited()) {
    timer_.destroy();
  }
  is_inited_ = false;
}

void ObAIColumnWorker::runTimerTask()
{
  int ret = OB_SUCCESS;
  ObCurTraceId::init(GCTX.self_addr());
  if (!is_inited_ || is_paused_ || OB_ISNULL(sql_proxy_)) {
  } else if (OB_FAIL(do_work())) {
    LOG_WARN("AI column worker iteration failed", K(ret));
  }
}

int ObAIColumnWorker::do_work()
{
  return process_next_group();
}

int ObAIColumnWorker::process_next_group()
{
  int ret = OB_SUCCESS;
  bool found = false;
  ObArenaAllocator allocator;
  ObSEArray<AITask, 8> tasks;
  if (OB_ISNULL(sql_proxy_)) {
    ret = OB_NOT_INIT;
  } else if (OB_FAIL(read_tasks(*sql_proxy_, allocator, tasks, found))) {
  } else if (!found) {
  } else if (OB_FAIL(mark_running(*sql_proxy_, tasks))) {
  } else {
    AITask *failed_task = nullptr;
    int model_ret = OB_SUCCESS;
    for (int64_t i = 0; i < tasks.count(); ++i) {
      AITask &task = tasks.at(i);
      if (OB_SUCCESS != (model_ret = evaluate_prompt(*sql_proxy_, allocator, task))
          || OB_SUCCESS != (model_ret = call_model(allocator, task))) {
        failed_task = &task;
        break;
      }
    }
    if (OB_NOT_NULL(failed_task)) {
      if (OB_FAIL(finish_failure(*sql_proxy_, *failed_task, tasks))) {
      }
    } else if (OB_FAIL(finish_success(*sql_proxy_, tasks))) {
      failed_task = &tasks.at(0);
      if (OB_FAIL(finish_failure(*sql_proxy_, *failed_task, tasks))) {
      }
    }
  }
  return ret;
}

} // namespace observer
} // namespace oceanbase
