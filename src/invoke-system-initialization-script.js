// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

// Invoke system for the Servo runtime, derived from tauri's ipc-protocol.js.
//
// Servo does not expose custom protocol request bodies to embedders, so the
// custom protocol IPC path used by the default invoke system cannot carry
// invoke payloads. This script always routes invokes through the
// `window.ipc.postMessage` bridge provided by tauri-runtime-servo, except for
// the channel data fetch command which carries its arguments in request
// headers (no body needed) and requires a response.

;(function () {
  /**
   * A runtime generated key to ensure an IPC call comes from an initialized frame.
   *
   * This is declared outside the `window.__TAURI_INVOKE__` definition to prevent
   * the key from being leaked by `window.__TAURI_INVOKE__.toString()`.
   */
  const __TAURI_INVOKE_KEY__ = __INVOKE_KEY__

  const processIpcMessage = function (message) {
    if (
      message instanceof ArrayBuffer
      || ArrayBuffer.isView(message)
      || Array.isArray(message)
    ) {
      return {
        contentType: 'application/octet-stream',
        data: message
      }
    } else {
      const data = JSON.stringify(message, (_k, val) => {
        // if this value changes, make sure to update it in:
        // 1. ipc.js
        // 2. core.ts
        const SERIALIZE_TO_IPC_FN = '__TAURI_TO_IPC_KEY__'

        if (val instanceof Map) {
          return Object.fromEntries(val.entries())
        } else if (val instanceof Uint8Array) {
          return Array.from(val)
        } else if (val instanceof ArrayBuffer) {
          return Array.from(new Uint8Array(val))
        } else if (
          typeof val === 'object'
          && val !== null
          && SERIALIZE_TO_IPC_FN in val
        ) {
          return val[SERIALIZE_TO_IPC_FN]()
        } else {
          return val
        }
      })

      return {
        contentType: 'application/json',
        data
      }
    }
  }

  const fetchChannelDataCommand = 'plugin:__TAURI_CHANNEL__|fetch'
  let customProtocolIpcFailed = false

  function sendIpcMessage(message) {
    const { cmd, callback, error, payload, options } = message

    if (!customProtocolIpcFailed && cmd === fetchChannelDataCommand) {
      // the channel data fetch passes its arguments via headers and needs the
      // response body, so it is the one command that keeps using the custom
      // protocol
      const { contentType, data } = processIpcMessage(payload)

      const headers = new Headers((options && options.headers) || {})
      headers.set('Content-Type', contentType)
      headers.set('Tauri-Callback', callback)
      headers.set('Tauri-Error', error)
      headers.set('Tauri-Invoke-Key', __TAURI_INVOKE_KEY__)

      fetch(window.__TAURI_INTERNALS__.convertFileSrc(cmd, 'ipc'), {
        method: 'POST',
        body: data,
        headers
      })
        .then((response) => {
          const callbackId =
            response.headers.get('Tauri-Response') === 'ok' ? callback : error
          switch ((response.headers.get('content-type') || '').split(',')[0]) {
            case 'application/json':
              return response.json().then((r) => [callbackId, r])
            case 'text/plain':
              return response.text().then((r) => [callbackId, r])
            default:
              return response.arrayBuffer().then((r) => [callbackId, r])
          }
        })
        .then(
          ([callbackId, data]) => {
            window.__TAURI_INTERNALS__.runCallback(callbackId, data)
          },
          (e) => {
            console.warn(
              'IPC custom protocol failed, Tauri will now use the postMessage interface instead',
              e
            )
            customProtocolIpcFailed = true
            sendIpcMessage(message)
          }
        )
    } else {
      // `window.ipc.postMessage` is provided by tauri-runtime-servo's IPC bridge
      const { data } = processIpcMessage({
        cmd,
        callback,
        error,
        options: {
          ...options,
          customProtocolIpcBlocked: customProtocolIpcFailed
        },
        payload,
        __TAURI_INVOKE_KEY__
      })
      window.ipc.postMessage(data)
    }
  }

  Object.defineProperty(window.__TAURI_INTERNALS__, 'postMessage', {
    value: sendIpcMessage
  })
})()
