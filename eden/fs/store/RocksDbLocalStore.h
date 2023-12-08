/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include <folly/CppAttributes.h>
#include <folly/Synchronized.h>
#include <bitset>

#include "eden/fs/rocksdb/RocksHandles.h"
#include "eden/fs/store/LocalStore.h"
#include "eden/fs/utils/UnboundedQueueExecutor.h"

namespace facebook::eden {

class FaultInjector;
class StructuredLogger;
class EdenStats;

using EdenStatsPtr = RefPtr<EdenStats>;

/** An implementation of LocalStore that uses RocksDB for the underlying
 * storage.
 */
class RocksDbLocalStore final : public LocalStore {
 public:
  /**
   * The given FaultInjector must be valid during the lifetime of this
   * RocksDbLocalStore object.
   */
  explicit RocksDbLocalStore(
      AbsolutePathPiece pathToRocksDb,
      EdenStatsPtr edenStats,
      std::shared_ptr<StructuredLogger> structuredLogger,
      FaultInjector* FOLLY_NONNULL faultInjector,
      RocksDBOpenMode mode = RocksDBOpenMode::ReadWrite);
  void open() override;
  ~RocksDbLocalStore();
  void close() override;
  void clearKeySpace(KeySpace keySpace) override;
  void compactKeySpace(KeySpace keySpace) override;
  StoreResult get(KeySpace keySpace, folly::ByteRange key) const override;
  FOLLY_NODISCARD ImmediateFuture<std::vector<StoreResult>> getBatch(
      KeySpace keySpace,
      const std::vector<folly::ByteRange>& keys) const override;
  bool hasKey(KeySpace keySpace, folly::ByteRange key) const override;
  void put(KeySpace keySpace, folly::ByteRange key, folly::ByteRange value)
      override;
  std::unique_ptr<WriteBatch> beginWrite(size_t bufSize = 0) override;

  // Call RocksDB's RepairDB() function on the DB at the specified location
  static void repairDB(AbsolutePathPiece path);

  // Get the approximate number of bytes stored on disk for the
  // specified key space.
  uint64_t getApproximateSize(KeySpace keySpace) const;

  void periodicManagementTask(const EdenConfig& config) override;

  enum class RockDbHandleStatus { NOT_YET_OPENED, OPEN, CLOSED };

  struct RockDBState {
    std::unique_ptr<RocksHandles> handles;
    RockDbHandleStatus status{RockDbHandleStatus::NOT_YET_OPENED};

    RockDBState();
  };

 private:
  /**
   * Get a pointer to the RocksHandles object in order to perform an I/O
   * operation.
   *
   * Note that even though this acquires a read-lock, write operations to the
   * DB may still be performed.  The lock exists to prevent the DB from being
   * closed while the I/O operation is in progress.
   */
  folly::Synchronized<RockDBState>::ConstRLockedPtr getHandles() const;
  [[noreturn]] void throwStoreClosedError(
      folly::Synchronized<RocksDbLocalStore::RockDBState>::ConstRLockedPtr&
          lockedState) const;
  [[noreturn]] void throwStoreNotYetOpenedError(
      folly::Synchronized<RocksDbLocalStore::RockDBState>::ConstRLockedPtr&
          lockedState) const;
  std::shared_ptr<RocksDbLocalStore> getSharedFromThis() {
    return std::static_pointer_cast<RocksDbLocalStore>(shared_from_this());
  }
  std::shared_ptr<const RocksDbLocalStore> getSharedFromThis() const {
    return std::static_pointer_cast<const RocksDbLocalStore>(
        shared_from_this());
  }

  struct AutoGCState {
    bool inProgress_{false};
    std::chrono::steady_clock::time_point startTime_;
  };

  struct SizeSummary {
    /**
     * Total size of ephemeral columns.
     */
    uint64_t ephemeral = 0;
    /**
     * Total size of all persistent columns.
     */
    uint64_t persistent = 0;
    /**
     * Which keyspace indices exceed their configured size limit and should be
     * cleared.
     */
    std::bitset<KeySpace::kTotalCount> excessiveKeySpaces;
  };

  /**
   * Publish fb303 counters.
   * Returns the approximate sizes of all column families.
   */
  SizeSummary computeStats(bool publish, const EdenConfig* config);

  void triggerAutoGC(SizeSummary before);
  void autoGCFinished(bool successful, uint64_t ephemeralSizeBefore);

  std::shared_ptr<StructuredLogger> structuredLogger_;
  const std::string statsPrefix_{"local_store."};
  FaultInjector& faultInjector_;
  mutable UnboundedQueueExecutor ioPool_;
  folly::Synchronized<AutoGCState> autoGCState_;
  AbsolutePath pathToDb_;
  RocksDBOpenMode mode_;
  folly::Synchronized<RockDBState> dbHandles_;
};

} // namespace facebook::eden
