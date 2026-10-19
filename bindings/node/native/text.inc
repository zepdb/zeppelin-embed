// Included in addon.cc's private namespace; no Node objects cross a worker
// boundary.
#ifdef ZE_TEXT
bool TextBig(napi_env env, napi_value object, const char *name,
             uint64_t value) {
  napi_value result;
  return NapiOk(env, napi_create_bigint_uint64(env, value, &result),
                "text bigint") &&
         SetNamed(env, object, name, result);
}
bool TextNumber(napi_env env, napi_value object, const char *name,
                double value) {
  napi_value result;
  return NapiOk(env, napi_create_double(env, value, &result), "text number") &&
         SetNamed(env, object, name, result);
}
bool TextString(napi_env env, napi_value object, const char *name,
                const uint8_t *bytes, size_t length) {
  napi_value result;
  return NapiOk(env,
                napi_create_string_utf8(env,
                                        reinterpret_cast<const char *>(bytes),
                                        length, &result),
                "text string") &&
         SetNamed(env, object, name, result);
}
napi_value TextDefaultModel(napi_env env, napi_callback_info) {
  ZeTextModelRef model{};
  model.abi_size = sizeof(model);
  const ze_error_code status = ze_text_default_model(&model);
  if (status != ZE_OK) return ThrowWorkerError(env, status, LastError(0));
  napi_value result;
  if (!NapiOk(env, napi_create_object(env, &result), "default model") ||
      !TextString(env, result, "repo", model.repo, model.repo_len) ||
      !TextString(env, result, "revision", model.revision, model.revision_len) ||
      !TextString(env, result, "manifestSha256", model.manifest_sha256, model.manifest_sha256_len)) return nullptr;
  return result;
}
bool TextCount(napi_env env, napi_value object, const char *name,
               size_t fallback, size_t *out) {
  napi_value value;
  bool present;
  if (!GetNamed(env, object, name, &value, &present))
    return false;
  if (!present) {
    *out = fallback;
    return true;
  }
  double number;
  if (!NapiOk(env, napi_get_value_double(env, value, &number), "text count"))
    return false;
  if (!std::isfinite(number) || number < 0 || number > 9007199254740991.0 ||
      std::floor(number) != number) {
    napi_throw_range_error(env, "ERR_OUT_OF_RANGE", name);
    return false;
  }
  *out = static_cast<size_t>(number);
  return true;
}
bool TextRequiredString(napi_env env, napi_value object, const char *name,
                        std::string *out) {
  napi_value value;
  bool present;
  if (!GetNamed(env, object, name, &value, &present))
    return false;
  if (!present) {
    napi_throw_type_error(env, "ERR_MISSING_ARGS", name);
    return false;
  }
  return GetUtf8(env, value, name, out);
}
bool TextEnum(napi_env env, napi_value options, const char *name,
              const char *const *names, size_t count, int32_t *out) {
  std::string value;
  bool present;
  if (!GetOptionalString(env, options, name, &value, &present))
    return false;
  if (!present)
    return true;
  if (ParseEnum(value, names, count, out))
    return true;
  napi_throw_range_error(env, "ERR_OUT_OF_RANGE", name);
  return false;
}
struct TextProgress {
  uint64_t done, total;
};
void CallTextProgress(napi_env env, napi_value callback, void *, void *raw) {
  std::unique_ptr<TextProgress> progress(static_cast<TextProgress *>(raw));
  if (!env || !callback)
    return;
  napi_value event, receiver, ignored;
  if (!NapiOk(env, napi_create_object(env, &event), "progress event") ||
      !TextBig(env, event, "done", progress->done) ||
      !TextBig(env, event, "total", progress->total) ||
      !NapiOk(env, napi_get_undefined(env, &receiver), "progress receiver"))
    return;
  NapiOk(env, napi_call_function(env, receiver, callback, 1, &event, &ignored),
         "download progress");
}
struct TextOpenData {
  napi_env env;
  napi_ref constructor = nullptr;
  napi_threadsafe_function progress = nullptr;
  bool progress_closing = false;
  std::string path, bundle, repo, revision, sha, cache;
  ZeTextOpenRequest request{};
  ZeTextOpenModelRequest model{};
  ze_handle handle = 0;
  ze_error_code progress_status = ZE_OK;
  explicit TextOpenData(napi_env e) : env(e) {}
  ~TextOpenData() {
    if (handle)
      ze_close(handle);
    if (constructor)
      napi_delete_reference(env, constructor);
    if (progress && !progress_closing)
      napi_release_threadsafe_function(progress, napi_tsfn_release);
  }
};
void DownloadProgress(void *raw, uint64_t done, uint64_t total) {
  auto *data = static_cast<TextOpenData *>(raw);
  if (data->progress_closing)
    return;
  auto event = std::unique_ptr<TextProgress>(new (std::nothrow)
                                                 TextProgress{done, total});
  if (!event) {
    data->progress_status = ZE_ERR_OUT_OF_MEMORY;
    return;
  }
  const auto status = napi_call_threadsafe_function(data->progress, event.get(),
                                                    napi_tsfn_blocking);
  if (status == napi_ok)
    event.release();
  else {
    if (status == napi_closing)
      data->progress_closing = true;
    data->progress_status = ZE_ERR_INTERNAL;
  }
}
template <bool Async, bool Model = false>
napi_value TextOpen(napi_env env, napi_callback_info info) {
  return Guard(env, [&]() -> napi_value {
    size_t argc = 1;
    napi_value options, receiver, constructor;
    if (!NapiOk(
            env,
            napi_get_cb_info(env, info, &argc, &options, &receiver, nullptr),
            "text open arguments"))
      return nullptr;
    if (argc != 1) {
      napi_throw_type_error(env, "ERR_MISSING_ARGS",
                            "text open options required");
      return nullptr;
    }
    auto data = std::make_shared<TextOpenData>(env);
    if (!NapiOk(
            env,
            napi_get_named_property(env, receiver, "NativeStore", &constructor),
            "store constructor") ||
        !NapiOk(env,
                napi_create_reference(env, constructor, 1, &data->constructor),
                "retain constructor") ||
        !TextRequiredString(env, options, "path", &data->path))
      return nullptr;
    auto &request = data->request;
    request.abi_size = sizeof(request);
    if (!ParseOpenRequest(env, options, data->path, &request.store))
      return nullptr;
    if constexpr (Model) {
      auto &m = data->model;
      m.abi_size = sizeof(m);
      m.store = request.store;
      bool present, offline;
      if (!TextRequiredString(env, options, "repo", &data->repo) ||
          !TextRequiredString(env, options, "revision", &data->revision) ||
          !TextRequiredString(env, options, "manifestSha256", &data->sha) ||
          !GetOptionalString(env, options, "cacheDir", &data->cache,
                             &present) ||
          !GetOptionalBool(env, options, "offline", false, &offline))
        return nullptr;
      m.repo = reinterpret_cast<const uint8_t *>(data->repo.data());
      m.repo_len = data->repo.size();
      m.revision = reinterpret_cast<const uint8_t *>(data->revision.data());
      m.revision_len = data->revision.size();
      m.manifest_sha256 = reinterpret_cast<const uint8_t *>(data->sha.data());
      m.manifest_sha256_len = data->sha.size();
      m.cache_dir = reinterpret_cast<const uint8_t *>(data->cache.data());
      m.cache_dir_len = data->cache.size();
      m.offline = offline;
      napi_value callback, name;
      if (!GetNamed(env, options, "onProgress", &callback, &present))
        return nullptr;
      if (present) {
        if (!NapiOk(env,
                    napi_create_string_utf8(env, "zeppelin.download",
                                            NAPI_AUTO_LENGTH, &name),
                    "progress name") ||
            !NapiOk(env,
                    napi_create_threadsafe_function(
                        env, callback, nullptr, name, 16, 1, nullptr, nullptr,
                        nullptr, CallTextProgress, &data->progress),
                    "progress function"))
          return nullptr;
        m.progress = DownloadProgress;
        m.user_data = data.get();
      }
    } else {
      if (!TextRequiredString(env, options, "bundlePath", &data->bundle))
        return nullptr;
      request.bundle_path =
          reinterpret_cast<const uint8_t *>(data->bundle.data());
      request.bundle_path_len = data->bundle.size();
    }
    return RunNative<Async>(
        env, receiver, 0,
        [data]() {
          ze_error_code status;
          if constexpr (Model)
            status = ze_text_open_model(&data->model, &data->handle);
          else
            status = ze_text_open(&data->request, &data->handle);
          if (data->progress && !data->progress_closing) {
            napi_release_threadsafe_function(data->progress, napi_tsfn_release);
            data->progress = nullptr;
          }
          return status == ZE_OK ? data->progress_status : status;
        },
        [env, data](ze_error_code status,
                    const std::string &message) -> napi_value {
          if (status != ZE_OK)
            return ThrowWorkerError(env, status,
                                    data->progress_status != ZE_OK
                                        ? "download progress delivery failed"
                                        : message);
          napi_value ctor, result;
          if (!NapiOk(env,
                      napi_get_reference_value(env, data->constructor, &ctor),
                      "constructor") ||
              !NapiOk(env, napi_new_instance(env, ctor, 0, nullptr, &result),
                      "text store"))
            return nullptr;
          NativeStore *store = nullptr;
          if (!NapiOk(
                  env,
                  napi_unwrap(env, result, reinterpret_cast<void **>(&store)),
                  "text handle"))
            return nullptr;
          store->handle = data->handle;
          data->handle = 0;
          return result;
        });
  });
}

enum class TextOp { Ingest, Query, Delete, Maintain, Health, ObservedComputeUnits };
struct TextData {
  ze_handle handle;
  std::vector<std::string> texts;
  std::vector<ZeTextDocument> documents;
  std::vector<ZeDocId> ids;
  std::string text;
  ZeTextIngestRequestV2 ingest{};
  ZeTextIngestReport ingested{};
  ZeTextQueryRequestV2 query{};
  ZeTextQueryOutcome outcome{};
  ZeDeleteRequest deletion{};
  ZeMutationReport deleted{};
  ZeMaintainRequest maintenance{};
  ZeMaintainReport maintained{};
  ZeTextHealthReport health{};
  ZeTextObservedComputeUnits observed_compute_units{};
  bool owns_outcome = false;
  explicit TextData(ze_handle h) : handle(h) {}
  ~TextData() {
    if (owns_outcome)
      ze_text_query_outcome_free(&outcome);
  }
};
bool TextControl(napi_env env, napi_value options, uint64_t *token,
                 uint64_t *deadline) {
  return GetOptionalUint64(env, options, "cancelToken", 0, token) &&
         GetOptionalUint64(env, options, "deadlineNs", 0, deadline);
}
template <TextOp Op, bool Async = false>
napi_value TextCall(napi_env env, napi_callback_info info) {
  return Guard(env, [&]() -> napi_value {
    napi_value args[2], receiver;
    size_t argc = 2;
    if (!NapiOk(env,
                napi_get_cb_info(env, info, &argc, args, &receiver, nullptr),
                "text arguments"))
      return nullptr;
    NativeStore *store = UnwrapStore(env, receiver);
    if (!store)
      return nullptr;
    auto data = std::make_shared<TextData>(store->handle);
    if constexpr (Op != TextOp::Health && Op != TextOp::ObservedComputeUnits) {
      if (argc < 1) {
        napi_throw_type_error(env, "ERR_MISSING_ARGS",
                              "text operation requires arguments");
        return nullptr;
      }
    }
    if constexpr (Op == TextOp::Ingest || Op == TextOp::Delete) {
      bool array = false;
      uint32_t count;
      if (!NapiOk(env, napi_is_array(env, args[0], &array), "text documents"))
        return nullptr;
      if (!array) {
        napi_throw_type_error(env, "ERR_INVALID_ARG_TYPE",
                              "documents/ids must be an array");
        return nullptr;
      }
      if (!NapiOk(env, napi_get_array_length(env, args[0], &count),
                  "document count"))
        return nullptr;
      data->texts.resize(count);
      data->documents.resize(count);
      data->ids.resize(count);
      for (uint32_t i = 0; i < count; ++i) {
        napi_value item, id;
        if (!NapiOk(env, napi_get_element(env, args[0], i, &item),
                    "text document"))
          return nullptr;
        if constexpr (Op == TextOp::Delete) {
          if (!GetDocId(env, item, &data->ids[i]))
            return nullptr;
        } else {
          auto &doc = data->documents[i];
          doc.abi_size = sizeof(doc);
          if (!NapiOk(env, napi_get_named_property(env, item, "id", &id),
                      "document id") ||
              !GetDocId(env, id, &doc.doc_id) ||
              !GetOptionalUint64(env, item, "revision", 1, &doc.revision) ||
              !TextRequiredString(env, item, "text", &data->texts[i]))
            return nullptr;
          doc.text = reinterpret_cast<const uint8_t *>(data->texts[i].data());
          doc.text_len = data->texts[i].size();
        }
      }
      if constexpr (Op == TextOp::Ingest) {
        if (argc < 2) {
          napi_throw_type_error(env, "ERR_MISSING_ARGS",
                                "ingest options required");
          return nullptr;
        }
        auto &r = data->ingest;
        r.abi_size = sizeof(r);
        r.batch.abi_size = sizeof(r.batch);
        r.batch.documents = data->documents.data();
        r.batch.document_count = count;
        if (!TextCount(env, args[1], "embedBatchSize", 32,
                       &r.batch.embed_batch_size) ||
            !TextCount(env, args[1], "sealEvery", 4096, &r.batch.seal_every) ||
            !TextCount(env, args[1], "channelCapacity", 2,
                       &r.batch.channel_capacity) ||
            !TextCount(env, args[1], "chunkMax", 0, &r.chunk_max) ||
            !TextCount(env, args[1], "chunkOverlap", 0, &r.chunk_overlap) ||
            !TextControl(env, args[1], &r.cancel_token, &r.deadline_ns))
          return nullptr;
        r.chunk_policy = r.chunk_max ? 1 : 0;
        data->ingested.abi_size = sizeof(data->ingested);
      } else {
        auto &r = data->deletion;
        r.abi_size = sizeof(r);
        r.doc_ids = data->ids.data();
        r.doc_id_count = count;
        data->deleted.abi_size = sizeof(data->deleted);
      }
    } else if constexpr (Op == TextOp::Query) {
      auto &r = data->query;
      r.abi_size = sizeof(r);
      r.query.abi_size = sizeof(r.query);
      if (!TextRequiredString(env, args[0], "text", &data->text) ||
          !TextCount(env, args[0], "k", 10, &r.query.k) ||
          !TextControl(env, args[0], &r.cancel_token, &r.deadline_ns))
        return nullptr;
      r.query.text = reinterpret_cast<const uint8_t *>(data->text.data());
      r.query.text_len = data->text.size();
      const char *legs[] = {"dense", "lexical", "hybrid"},
                 *tiers[] = {"auto", "exact", "scan", "graph"};
      bool present;
      napi_value tier;
      if (!TextEnum(env, args[0], "legs", legs, 3, &r.query.legs) ||
          !GetNamed(env, args[0], "tier", &tier, &present) ||
          !TextEnum(env, args[0], "tier", tiers, 4, &r.tier))
        return nullptr;
      r.has_tier = present;
      data->outcome.abi_size = sizeof(data->outcome);
    } else if constexpr (Op == TextOp::Maintain) {
      auto &r = data->maintenance;
      r.abi_size = sizeof(r);
      if (!GetOptionalUint64(env, args[0], "wallTimeNs", 100000000,
                             &r.wall_time_ns) ||
          !GetOptionalUint64(env, args[0], "bytes", 67108864, &r.bytes))
        return nullptr;
      data->maintained.abi_size = sizeof(data->maintained);
    } else if constexpr (Op == TextOp::ObservedComputeUnits) {
      data->observed_compute_units.abi_size = sizeof(data->observed_compute_units);
    } else {
      data->health.abi_size = sizeof(data->health);
    }
    return RunNative<Async>(
        env, receiver, data->handle,
        [data]() {
          if constexpr (Op == TextOp::Ingest)
            return ze_text_ingest_v2(data->handle, &data->ingest,
                                     &data->ingested);
          if constexpr (Op == TextOp::Query) {
            auto status =
                ze_text_query_v2(data->handle, &data->query, &data->outcome);
            data->owns_outcome = status == ZE_OK;
            return status;
          }
          if constexpr (Op == TextOp::Delete)
            return ze_text_delete(data->handle, &data->deletion,
                                  &data->deleted);
          if constexpr (Op == TextOp::Maintain)
            return ze_text_maintain_budgeted(data->handle, &data->maintenance,
                                             &data->maintained);
          if constexpr (Op == TextOp::Health)
            return ze_text_health(data->handle, &data->health);
          if constexpr (Op == TextOp::ObservedComputeUnits)
            return ze_text_observed_compute_units(data->handle,
                                                  &data->observed_compute_units);
        },
        [data, env](ze_error_code status,
                    const std::string &message) -> napi_value {
          if (status != ZE_OK)
            return ThrowWorkerError(env, status, message);
          napi_value result;
          if (!NapiOk(env, napi_create_object(env, &result), "text result"))
            return nullptr;
          if constexpr (Op == TextOp::Ingest) {
            const auto &r = data->ingested;
            if (!TextBig(env, result, "generation", r.generation) ||
                !TextBig(env, result, "documents", r.documents) ||
                !TextBig(env, result, "chunks", r.chunks) ||
                !TextBig(env, result, "tokens", r.tokens) ||
                !TextBig(env, result, "embedBatches", r.embed_batches) ||
                !TextBig(env, result, "seals", r.seals) ||
                !TextBig(env, result, "maxInFlightBatches",
                         r.max_in_flight_batches) ||
                !TextBig(env, result, "elapsedNs", r.elapsed_ns))
              return nullptr;
            napi_value joined;
            if (!NapiOk(env,
                        napi_get_boolean(env, r.all_threads_joined, &joined),
                        "joined") ||
                !SetNamed(env, result, "allThreadsJoined", joined))
              return nullptr;
          } else if constexpr (Op == TextOp::Query) {
            const auto &r = data->outcome.result;
            napi_value hits;
            if (!NapiOk(env,
                        napi_create_array_with_length(env, r.hit_count, &hits),
                        "text hits"))
              return nullptr;
            for (size_t i = 0; i < r.hit_count; ++i) {
              const auto &h = r.hits[i];
              napi_value hit, id;
              if (!NapiOk(env, napi_create_object(env, &hit), "text hit") ||
                  !CreateUint128(env, h.doc_id, &id) ||
                  !SetNamed(env, hit, "id", id) ||
                  !TextBig(env, hit, "revision", h.revision) ||
                  !TextNumber(env, hit, "chunk", h.chunk) ||
                  !TextNumber(env, hit, "score", h.score) ||
                  !TextString(env, hit, "text", h.text, h.text_len) ||
                  (h.has_vector_score &&
                   !TextNumber(env, hit, "vectorSquaredL2",
                               h.vector_squared_l2)) ||
                  (h.has_lexical_score &&
                   !TextNumber(env, hit, "lexicalBm25", h.lexical_bm25)) ||
                  !NapiOk(env, napi_set_element(env, hits, i, hit),
                          "append text hit"))
                return nullptr;
            }
            if (!SetNamed(env, result, "hits", hits) ||
                !TextBig(env, result, "embeddingEpoch", r.embedding_epoch) ||
                !TextBig(env, result, "tokenizerEpoch", r.tokenizer_epoch))
              return nullptr;
          } else if constexpr (Op == TextOp::Delete) {
            if (!TextBig(env, result, "sequence", data->deleted.sequence) ||
                !TextBig(env, result, "generation", data->deleted.generation))
              return nullptr;
          } else if constexpr (Op == TextOp::Maintain) {
            const auto &r = data->maintained;
            if (!TextNumber(env, result, "status", r.status) ||
                !TextBig(env, result, "graphsBuilt", r.graphs_built) ||
                !TextBig(env, result, "bytesConsumed", r.bytes_consumed) ||
                !TextBig(env, result, "checkpointsResumed",
                         r.checkpoints_resumed))
              return nullptr;
          } else if constexpr (Op == TextOp::ObservedComputeUnits) {
            const auto &r = data->observed_compute_units;
            if (!r.has_observed_compute_units) {
              if (!NapiOk(env, napi_get_null(env, &result), "compute units absent"))
                return nullptr;
            } else if (!TextBig(env, result, "cpuOps", r.cpu_ops) ||
                       !TextBig(env, result, "gpuOps", r.gpu_ops) ||
                       !TextBig(env, result, "neuralEngineOps", r.neural_engine_ops) ||
                       !TextBig(env, result, "otherOps", r.other_ops)) {
              return nullptr;
            }
          } else {
            const auto &r = data->health;
            if (!TextBig(env, result, "generation", r.generation) ||
                !TextNumber(env, result, "status", r.status) ||
                !TextBig(env, result, "pendingDocs", r.pending_docs) ||
                !TextNumber(env, result, "graphCoverage", r.graph_coverage) ||
                !TextBig(env, result, "segmentCount", r.segment_count) ||
                !TextBig(env, result, "unresolvedFaultCount",
                         r.unresolved_fault_count))
              return nullptr;
            if (r.has_retained_through) {
              napi_value retained;
              if (!NapiOk(env,
                          napi_create_bigint_int64(env, r.retained_through,
                                                   &retained),
                          "retained through") ||
                  !SetNamed(env, result, "retainedThrough", retained))
                return nullptr;
            }
          }
          return result;
        });
  });
}
#endif
