// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_POWER_TESTING_FAKE_POWERSOURCE_POWER_SOURCE_STATE_H_
#define SRC_POWER_TESTING_FAKE_POWERSOURCE_POWER_SOURCE_STATE_H_

#include <fidl/fuchsia.hardware.powersource/cpp/fidl.h>

#include <unordered_set>

namespace fake_powersource {

// This abstract class just defines an interface for observers to be notified.
class Observer {
 public:
  virtual void Notify() = 0;
};

// A state of data corresponding to one power node (and / or one simulator node).
class PowerSourceState {
 public:
  explicit PowerSourceState(fuchsia_hardware_powersource::SourceInfo info);
  // The PowerSourceState lives longer than the observers since the observers are server instances
  // which can be spawned and destroyed. Therefore class PowerSourceState doesn't own observers.
  // The observer must live until it calls RemoveObserver to remove itself from the observers_.
  // i.e., the observers when being destructed, must call RemoveObserver.
  void AddObserver(Observer* observer);
  void RemoveObserver(Observer* observer);

  // Notify the observers, and then clear the container.
  void NotifyObservers();

  void set_battery_info(const fuchsia_hardware_powersource::BatteryInfo& info);
  fuchsia_hardware_powersource::BatteryInfo battery_info() const { return battery_info_; }
  fuchsia_hardware_powersource::SourceInfo source_info() const { return source_info_; }

 private:
  fuchsia_hardware_powersource::BatteryInfo battery_info_{{
      .unit = fuchsia_hardware_powersource::BatteryUnit::kMa,
      .design_capacity = 3000,
      .last_full_capacity = 2950,
      .design_voltage = 3000,  // mV
      .capacity_warning = 800,
      .capacity_low = 500,
      .capacity_granularity_low_warning = 20,
      .capacity_granularity_warning_full = 1,
      .present_rate = 2,
      .remaining_capacity = 2950,
      .present_voltage = 2910,
      .battery_spec = fuchsia_hardware_powersource::BatterySpec{{
          .max_charging_current_ua = 1000000,
          .max_charging_voltage_uv = 5000000,
          .design_capacity_uah = 300000,
      }},
  }};
  fuchsia_hardware_powersource::SourceInfo source_info_;
  std::unordered_set<Observer*> observers_;
};

}  // namespace fake_powersource

#endif  // SRC_POWER_TESTING_FAKE_POWERSOURCE_POWER_SOURCE_STATE_H_
