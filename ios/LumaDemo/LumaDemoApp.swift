//
//  LumaDemoApp.swift
//  A live BLE demo of the `luma-core` crate, driven entirely through its
//  generated Swift bindings.
//
//  The Simulator has no Bluetooth radio: this builds there, but it can only connect
//  on a physical iPhone.
//

import SwiftUI

@main
struct LumaDemoApp: App {
    var body: some Scene {
        WindowGroup {
            ContentView()
        }
    }
}
