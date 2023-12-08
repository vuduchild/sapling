/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include <memory>
#include "eden/fs/utils/ImmediateFuture.h"

namespace facebook::eden {

class Blob;
class ObjectId;
class Tree;

template <typename T>
class StoredObject;
using StoredBlob = StoredObject<const Blob>;
using StoredHash = StoredObject<ObjectId>;
using StoredTree = StoredObject<const Tree>;

/**
 * A helper class for TestBackingStore.
 *
 * This contains a Tree, Blob, or Hash, but allows tracking when it should
 * actually be marked ready to return to callers.  The getFuture() API can be
 * used to get a folly::Future that will be fulfilled when the object is marked
 * ready.
 *
 * This allows test code to test the code behavior when backing store objects
 * are not immediately ready.
 */
template <typename T>
class StoredObject {
 public:
  explicit StoredObject(const T& t) : object_(t) {}

  /**
   * Get the underlying object.
   */
  const T& get() const {
    return object_;
  }

  /**
   * Get a Future for this object.
   *
   * If the StoredObject is ready, the returned future will already have a
   * value available.  Otherwise the future will become ready when trigger() or
   * setReady() is called on this StoredObject.
   */
  ImmediateFuture<std::shared_ptr<T>> getFuture() {
    auto data = data_.wlock();
    if (data->ready) {
      return folly::makeFuture(std::make_shared<T>(object_));
    }

    data->promises.emplace_back();
    return data->promises.back().getSemiFuture();
  }

  /**
   * Mark the object as ready.
   *
   * This will fulfill any pending Futures waiting on this object.
   * New Futures returned by getFuture() after setReady() is called will be
   * immediately ready.
   */
  void setReady() {
    std::vector<folly::Promise<std::shared_ptr<T>>> promises;
    {
      auto data = data_.wlock();
      data->ready = true;
      data->promises.swap(promises);
    }
    triggerImpl(promises);
  }

  /**
   * Mark an object as not ready again.
   *
   * Subsequent requests to access it will block until setReady() or trigger()
   * is called again.
   */
  void notReady() {
    auto data = data_.wlock();
    data->ready = false;
  }

  /**
   * Fulfill all pending Futures waiting on this object.
   *
   * This fulfills currently pending Futures, but subsequent calls to
   * getFuture() will still return Futures that are not ready yet.
   */
  void trigger() {
    std::vector<folly::Promise<std::shared_ptr<T>>> promises;
    {
      auto data = data_.wlock();
      data->promises.swap(promises);
    }
    triggerImpl(promises);
  }

  /**
   * Fail all pending Futures waiting on this object.
   *
   * This fulfills currently pending Futures with the specified exception.
   */
  template <class E>
  void triggerError(const E& e) {
    std::vector<folly::Promise<std::shared_ptr<T>>> promises;
    {
      auto data = data_.wlock();
      data->promises.swap(promises);
    }

    for (auto& p : promises) {
      p.setException(e);
    }
  }

  std::vector<folly::Promise<std::shared_ptr<T>>> discardOutstandingRequests() {
    std::vector<folly::Promise<std::shared_ptr<T>>> promises;
    {
      auto data = data_.wlock();
      // Destroying a Promise can run callbacks, so do that while the lock isn't
      // held.
      std::swap(promises, data->promises);
    }
    return promises;
  }

 private:
  struct Data {
    bool ready{false};
    std::vector<folly::Promise<std::shared_ptr<T>>> promises;
  };

  void triggerImpl(std::vector<folly::Promise<std::shared_ptr<T>>>& promises) {
    for (auto& p : promises) {
      p.setValue(std::make_shared<T>(object_));
    }
  }

  const T object_;
  folly::Synchronized<Data> data_;
};
} // namespace facebook::eden
