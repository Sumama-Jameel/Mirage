//! Human-like interaction and anti-detection JavaScript utilities for GLM.
//!
//! This module provides JavaScript that makes Obscura's headless browser behave
//! more like a real human browser to bypass Aliyun NVC captcha checks.

/// Generate a human-like Bézier curve mouse movement path.
pub const HUMAN_MOUSE_MOVE_JS: &str = r#"
(function() {
    window.__obscura_humanize = {
        /**
         * Generate a human-like mouse movement using Bézier curves.
         * Humans move mice with sin-wave wobble, not straight lines.
         */
        generateTrack: function(startX, startY, endX, endY, duration) {
            var points = [];
            var controlPoints = 8 + Math.floor(Math.random() * 5);
            
            for (var i = 0; i <= controlPoints; i++) {
                var t = i / controlPoints;
                var x = startX + (endX - startX) * t;
                // Human Y movement has sin-wave wobble
                var wobble = Math.sin(t * Math.PI * (2 + Math.random() * 2)) * (15 + Math.random() * 10);
                wobble += (Math.random() - 0.5) * 10;
                var y = startY + (endY - startY) * t + wobble;
                // Variable timing
                var timeOffset = Math.random() * 50;
                points.push({ x: x, y: y, delay: 16 + timeOffset });
            }
            
            return points;
        },

        /**
         * Move mouse along generated track using pointer events.
         */
        mouseMove: function(targetX, targetY, duration) {
            var self = this;
            return new Promise(function(resolve, reject) {
                var startX = targetX + (Math.random() - 0.5) * 100;
                var startY = targetY + (Math.random() - 0.5) * 100;
                
                var track = self.generateTrack(startX, startY, targetX, targetY, duration);
                var index = 0;
                
                function moveNext() {
                    if (index >= track.length) {
                        var opts = {
                            bubbles: true, cancelable: true, composed: true,
                            view: window, clientX: targetX, clientY: targetY,
                            screenX: targetX, screenY: targetY,
                            pointerId: 1, pointerType: 'mouse', isPrimary: true
                        };
                        window.dispatchEvent(new PointerEvent('pointermove', opts));
                        resolve();
                        return;
                    }
                    
                    var pt = track[index];
                    var opts = {
                        bubbles: true, cancelable: true, composed: true,
                        view: window, clientX: pt.x, clientY: pt.y,
                        screenX: pt.x, screenY: pt.y,
                        pointerId: 1, pointerType: 'mouse', isPrimary: true
                    };
                    window.dispatchEvent(new PointerEvent('pointermove', opts));
                    
                    index++;
                    setTimeout(moveNext, pt.delay);
                }
                
                // Initial pause before moving (200-400ms)
                setTimeout(moveNext, 200 + Math.random() * 200);
            });
        },

        /**
         * Perform a human-like click on an element.
         */
        click: function(element) {
            var self = this;
            return new Promise(function(resolve, reject) {
                var rect = element.getBoundingClientRect();
                var targetX = rect.left + rect.width / 2;
                var targetY = rect.top + rect.height / 2;
                
                // Random delay before clicking (300-600ms)
                setTimeout(async function() {
                    // Move mouse with Bézier curve (400-700ms)
                    await self.mouseMove(targetX, targetY, 400 + Math.random() * 300);
                    
                    // Small pause at target (50-150ms)
                    await new Promise(function(r) { setTimeout(r, 50 + Math.random() * 100); });
                    
                    // Click events with natural timing
                    var opts = {
                        bubbles: true, cancelable: true, composed: true,
                        view: window, clientX: targetX, clientY: targetY,
                        screenX: targetX, screenY: targetY,
                        pointerId: 1, pointerType: 'mouse', isPrimary: true
                    };
                    
                    element.dispatchEvent(new PointerEvent('pointerdown', opts));
                    await new Promise(function(r) { setTimeout(r, 50 + Math.random() * 50); });
                    element.dispatchEvent(new PointerEvent('pointerup', opts));
                    await new Promise(function(r) { setTimeout(r, 30 + Math.random() * 30); });
                    element.dispatchEvent(new MouseEvent('mousedown', opts));
                    await new Promise(function(r) { setTimeout(r, 50 + Math.random() * 50); });
                    element.dispatchEvent(new MouseEvent('mouseup', opts));
                    await new Promise(function(r) { setTimeout(r, 30 + Math.random() * 30); });
                    element.dispatchEvent(new MouseEvent('click', opts));
                    
                    resolve(true);
                }, 300 + Math.random() * 300);
            });
        },

        /**
         * Type text with human-like keystroke timing.
         */
        typeText: async function(input, text) {
            input.focus();
            
            for (var i = 0; i < text.length; i++) {
                var char = text[i];
                var keydown = new KeyboardEvent('keydown', {
                    bubbles: true, cancelable: true, key: char, code: 'Key' + char.toUpperCase()
                });
                input.dispatchEvent(keydown);
                
                // Set value via prototype to trigger React/Vue listeners
                var proto = input.tagName === 'TEXTAREA' || input.tagName === 'INPUT'
                    ? input.tagName === 'TEXTAREA' ? window.HTMLTextAreaElement.prototype
                    : window.HTMLInputElement.prototype
                    : window.HTMLElement.prototype;
                var setter = Object.getOwnPropertyDescriptor(proto, 'value').set;
                setter.call(input, input.value + char);
                
                var inputEvent = new Event('input', { bubbles: true, cancelable: true });
                input.dispatchEvent(inputEvent);
                
                // Random delay between keystrokes (50-150ms)
                await new Promise(function(r) { setTimeout(r, 50 + Math.random() * 100); });
            }
            
            return true;
        }
    };
    
    console.log('[Obscura] Humanize module loaded');
})();
"#;

/// Anti-detection initialization script that spoofs browser fingerprints.
pub const ANTI_DETECTION_INIT_JS: &str = r#"
(function() {
    'use strict';
    
    if (window.__obscura_anti_detect_initialized) return;
    window.__obscura_anti_detect_initialized = true;
    
    // ========== 1. CANVAS NOISE ==========
    (function() {
        var noiseIntensity = 5 + Math.random() * 10;
        
        var origGetImageData = CanvasRenderingContext2D.prototype.getImageData;
        CanvasRenderingContext2D.prototype.getImageData = function(sx, sy, sw, sh) {
            var result = origGetImageData.apply(this, arguments);
            try {
                var data = result.data;
                for (var i = 0; i < data.length; i += 4) {
                    data[i] = Math.min(255, Math.max(0, data[i] + (Math.random() - 0.5) * noiseIntensity * 2));
                    data[i+1] = Math.min(255, Math.max(0, data[i+1] + (Math.random() - 0.5) * noiseIntensity * 2));
                    data[i+2] = Math.min(255, Math.max(0, data[i+2] + (Math.random() - 0.5) * noiseIntensity * 2));
                }
            } catch(e) {}
            return result;
        };
        
        var origPutImageData = CanvasRenderingContext2D.prototype.putImageData;
        CanvasRenderingContext2D.prototype.putImageData = function(imageData, dx, dy) {
            try {
                var data = imageData.data;
                for (var i = 0; i < data.length; i += 4) {
                    data[i] = Math.min(255, Math.max(0, data[i] + (Math.random() - 0.5) * noiseIntensity * 2));
                    data[i+1] = Math.min(255, Math.max(0, data[i+1] + (Math.random() - 0.5) * noiseIntensity * 2));
                    data[i+2] = Math.min(255, Math.max(0, data[i+2] + (Math.random() - 0.5) * noiseIntensity * 2));
                }
            } catch(e) {}
            return origPutImageData.apply(this, arguments);
        };
        
        // toDataURL noise
        var origToDataURL = HTMLCanvasElement.prototype.toDataURL;
        HTMLCanvasElement.prototype.toDataURL = function() {
            try {
                var ctx = this.getContext('2d');
                if (ctx) {
                    var imageData = ctx.getImageData(0, 0, this.width, this.height);
                    var data = imageData.data;
                    for (var i = 0; i < data.length; i += 4) {
                        data[i] = Math.min(255, Math.max(0, data[i] + (Math.random() - 0.5) * noiseIntensity * 2));
                        data[i+1] = Math.min(255, Math.max(0, data[i+1] + (Math.random() - 0.5) * noiseIntensity * 2));
                        data[i+2] = Math.min(255, Math.max(0, data[i+2] + (Math.random() - 0.5) * noiseIntensity * 2));
                    }
                    ctx.putImageData(imageData, 0, 0);
                }
            } catch(e) {}
            return origToDataURL.apply(this, arguments);
        };
    })();
    
    // ========== 2. NAVIGATOR SPOOFING ==========
    (function() {
        // Remove webdriver flag
        Object.defineProperty(navigator, 'webdriver', {
            get: function() { return false; },
            configurable: true, enumerable: true
        });
        
        // Spoof hardware concurrency (8-16 cores)
        Object.defineProperty(navigator, 'hardwareConcurrency', {
            get: function() { return 8 + Math.floor(Math.random() * 9); },
            configurable: true, enumerable: true
        });
        
        // Spoof device memory (4-16 GB)
        Object.defineProperty(navigator, 'deviceMemory', {
            get: function() { return 4 + Math.floor(Math.random() * 13); },
            configurable: true, enumerable: true
        });
        
        // Remove automation globals
        window.callPhantom = undefined;
        window._phantom = undefined;
        window.__nightmare = undefined;
        window.__webdriver = undefined;
        window.selenide = undefined;
        window.browserless = undefined;
        window.puppeteer = undefined;
        window.playwright = undefined;
        
        // Fake plugins array
        Object.defineProperty(navigator, 'plugins', {
            get: function() {
                return [
                    { name: 'Chrome PDF Plugin', filename: 'internal-pdf-viewer', description: 'Portable Document Format viewer' },
                    { name: 'Chrome PDF Viewer', filename: 'mhjfbmdgcfjbbpaeojofohoefgiehjai', description: '' },
                    { name: 'Native Client', filename: 'internal-nacl-plugin', description: '' }
                ];
            },
            configurable: true, enumerable: true
        });
        
        // Fake chrome runtime
        window.chrome = {
            runtime: { 
                connect: function() { return { onMessage: { addListener: function() {} }, onDisconnect: { addListener: function() {} }, postMessage: function() {} }; },
                sendMessage: function() { return { then: function() { return this; }, catch: function() { return this; }; }; },
                id: ''
            },
            loadTimes: function() { return {}; },
            csi: function() { return {}; },
            app: {}
        };
        
        // Permissions API spoof
        var permissionsQuery = window.PermissionStatus && window.PermissionQuery;
        if (permissionsQuery) {
            // Already spoofed
        }
        navigator.permissions && Object.defineProperty(navigator.permissions, 'query', {
            value: function(parameters) {
                return Promise.resolve({ state: 'granted', addEventListener: function() {}, removeEventListener: function() {} });
            },
            configurable: true
        });
    })();
    
    // ========== 3. WEBGL SPOOFING ==========
    (function() {
        var getParameter = WebGLRenderingContext.prototype.getParameter;
        WebGLRenderingContext.prototype.getParameter = function(param) {
            if (param === 37445) return 'Intel Open Source Technology Center';
            if (param === 37446) return 'Mesa DRI Intel(R) UHD Graphics 620';
            return getParameter.apply(this, arguments);
        };
        
        if (typeof WebGL2RenderingContext !== 'undefined') {
            var getParameter2 = WebGL2RenderingContext.prototype.getParameter;
            WebGL2RenderingContext.prototype.getParameter = function(param) {
                if (param === 37445) return 'Intel Open Source Technology Center';
                if (param === 37446) return 'Mesa DRI Intel(R) UHD Graphics 620';
                return getParameter2.apply(this, arguments);
            };
        }
    })();
    
    // ========== 4. AUDIO CONTEXT SPOOFING ==========
    (function() {
        try {
            var AudioContext = window.AudioContext || window.webkitAudioContext;
            if (AudioContext) {
                var origCreateOscillator = AudioContext.prototype.createOscillator;
                AudioContext.prototype.createOscillator = function() {
                    var osc = origCreateOscillator.apply(this, arguments);
                    try {
                        var origGet = osc.frequency.getValue;
                        Object.defineProperty(osc.frequency, 'value', {
                            get: function() { return origGet.call(this); },
                            set: function(v) { 
                                var offset = (Math.random() - 0.5) * 0.02;
                                origGet.call(this, v + offset); 
                            },
                            configurable: true
                        });
                    } catch(e) {}
                    return osc;
                };
            }
        } catch(e) {}
    })();
    
    // ========== 5. SCREEN PROPERTIES SPOOFING ==========
    (function() {
        Object.defineProperty(screen, 'width', { get: function() { return 1920; }, configurable: true });
        Object.defineProperty(screen, 'height', { get: function() { return 1080; }, configurable: true });
        Object.defineProperty(screen, 'availWidth', { get: function() { return 1920; }, configurable: true });
        Object.defineProperty(screen, 'availHeight', { get: function() { return 1040; }, configurable: true });
        Object.defineProperty(screen, 'colorDepth', { get: function() { return 24; }, configurable: true });
        Object.defineProperty(screen, 'pixelDepth', { get: function() { return 24; }, configurable: true });
        
        Object.defineProperty(window, 'innerWidth', { get: function() { return 1920; }, configurable: true });
        Object.defineProperty(window, 'innerHeight', { get: function() { return 969; }, configurable: true });
        Object.defineProperty(window, 'outerWidth', { get: function() { return 1920; }, configurable: true });
        Object.defineProperty(window, 'outerHeight', { get: function() { return 1080; }, configurable: true });
    })();
    
    // ========== 6. TIMING SPOOFING ==========
    (function() {
        var originalNow = performance.now.bind(performance);
        var timeOffset = (Math.random() - 0.5) * 100;
        performance.now = function() {
            return originalNow() + timeOffset;
        };
    })();
    
    console.log('[Obscura] Anti-detection initialized');
})();
"#;

/// Script to inject before captcha solving - adds extra stealth for the captcha widget.
pub const CAPTCHA_STEALTH_INIT_JS: &str = r#"
(function() {
    'use strict';
    
    if (window.__obscura_captcha_stealth_initialized) return;
    window.__obscura_captcha_stealth_initialized = true;
    
    // Add Bézier mouse movement capability to captcha trigger clicks
    window.__obscura_humanize = window.__obscura_humanize || {
        generateTrack: function(startX, startY, endX, endY, duration) {
            var points = [];
            var controlPoints = 8 + Math.floor(Math.random() * 5);
            for (var i = 0; i <= controlPoints; i++) {
                var t = i / controlPoints;
                var x = startX + (endX - startX) * t;
                var wobble = Math.sin(t * Math.PI * (2 + Math.random() * 2)) * (15 + Math.random() * 10);
                wobble += (Math.random() - 0.5) * 10;
                var y = startY + (endY - startY) * t + wobble;
                points.push({ x: x, y: y, delay: 16 + Math.random() * 50 });
            }
            return points;
        },
        
        mouseMove: function(targetX, targetY, duration) {
            var self = this;
            return new Promise(function(resolve) {
                var startX = targetX + (Math.random() - 0.5) * 100;
                var startY = targetY + (Math.random() - 0.5) * 100;
                var track = self.generateTrack(startX, startY, targetX, targetY, duration);
                var index = 0;
                function moveNext() {
                    if (index >= track.length) {
                        window.dispatchEvent(new PointerEvent('pointermove', {
                            bubbles: true, cancelable: true, composed: true, view: window,
                            clientX: targetX, clientY: targetY, screenX: targetX, screenY: targetY,
                            pointerId: 1, pointerType: 'mouse', isPrimary: true
                        }));
                        resolve(); return;
                    }
                    var pt = track[index];
                    window.dispatchEvent(new PointerEvent('pointermove', {
                        bubbles: true, cancelable: true, composed: true, view: window,
                        clientX: pt.x, clientY: pt.y, screenX: pt.x, screenY: pt.y,
                        pointerId: 1, pointerType: 'mouse', isPrimary: true
                    }));
                    index++;
                    setTimeout(moveNext, pt.delay);
                }
                setTimeout(moveNext, 200 + Math.random() * 200);
            });
        },
        
        click: function(element) {
            var self = this;
            return new Promise(function(resolve) {
                var rect = element.getBoundingClientRect();
                var targetX = rect.left + rect.width / 2;
                var targetY = rect.top + rect.height / 2;
                setTimeout(async function() {
                    await self.mouseMove(targetX, targetY, 400 + Math.random() * 300);
                    await new Promise(function(r) { setTimeout(r, 50 + Math.random() * 100); });
                    var opts = {
                        bubbles: true, cancelable: true, composed: true, view: window,
                        clientX: targetX, clientY: targetY, screenX: targetX, screenY: targetY,
                        pointerId: 1, pointerType: 'mouse', isPrimary: true
                    };
                    element.dispatchEvent(new PointerEvent('pointerdown', opts));
                    await new Promise(function(r) { setTimeout(r, 50 + Math.random() * 50); });
                    element.dispatchEvent(new PointerEvent('pointerup', opts));
                    await new Promise(function(r) { setTimeout(r, 30 + Math.random() * 30); });
                    element.dispatchEvent(new MouseEvent('mousedown', opts));
                    await new Promise(function(r) { setTimeout(r, 50 + Math.random() * 50); });
                    element.dispatchEvent(new MouseEvent('mouseup', opts));
                    await new Promise(function(r) { setTimeout(r, 30 + Math.random() * 30); });
                    element.dispatchEvent(new MouseEvent('click', opts));
                    resolve(true);
                }, 300 + Math.random() * 300);
            });
        }
    };
    
    console.log('[Obscura] Captcha stealth module loaded');
})();
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_mouse_js_is_valid() {
        // Verify the JS doesn't have obvious syntax errors
        assert!(HUMAN_MOUSE_MOVE_JS.contains("generateTrack"));
        assert!(HUMAN_MOUSE_MOVE_JS.contains("mouseMove"));
        assert!(HUMAN_MOUSE_MOVE_JS.contains("click"));
    }

    #[test]
    fn anti_detection_js_is_valid() {
        assert!(ANTI_DETECTION_INIT_JS.contains("noiseIntensity"));
        assert!(ANTI_DETECTION_INIT_JS.contains("navigator"));
        assert!(ANTI_DETECTION_INIT_JS.contains("webdriver"));
        assert!(ANTI_DETECTION_INIT_JS.contains("WebGL"));
    }

    #[test]
    fn captcha_stealth_js_is_valid() {
        assert!(CAPTCHA_STEALTH_INIT_JS.contains("mouseMove"));
        assert!(CAPTCHA_STEALTH_INIT_JS.contains("click"));
    }
}
