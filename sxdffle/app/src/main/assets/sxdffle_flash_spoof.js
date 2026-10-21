/*
 * SXDffle 文档起手注入脚本（document-start）
 * =====================================================================
 * 必须在页面任何脚本执行之前注入，原因是：
 *
 * 页游普遍用 `navigator.plugins['Shockwave Flash']` 来判断"用户有没有装 Flash"。
 * 例如《神仙道》的 common.js 里有：
 *
 *     function detect_flash(){
 *         var isIE = !-[1,];
 *         if(isIE){ try{ new ActiveXObject('ShockwaveFlash.ShockwaveFlash'); return true; }catch(e){} }
 *         else { try{ var swf2 = navigator.plugins['Shockwave Flash'];
 *                     if(swf2 !== undefined){ return true; } }catch(e){} }
 *         return false;
 *     }
 *     ...
 *     var installFlash = detect_flash();     // ← 结果决定后续行为
 *
 * 由于 Flash 插件已从 Chromium 内核中彻底移除，这个值永远是 undefined，
 * 于是页面会认定"未安装 Flash"而拒绝渲染游戏入口 —— 即便我们的引擎
 * 完全能跑 Flash 也没用。
 *
 * 因此本脚本在最早的时机伪造出一套可信的 Flash 插件信息，
 * 让这类检测通过。伪造对象结构对齐真实 Flash Player 32。
 * =====================================================================
 */
(function () {
    'use strict';

    // 只注入一次
    if (window.__SXDFFLE_EARLY__) return;
    window.__SXDFFLE_EARLY__ = true;

    var FLASH_VERSION = '32.0.0.465';
    var FLASH_VERSION_MAJOR = 32;

    function makeMimeType(type, suffixes, description, enabledPlugin) {
        return {
            type: type,
            suffixes: suffixes,
            description: description,
            enabledPlugin: enabledPlugin
        };
    }

    // 构造一个"看起来像"的 Plugin 对象
    var flashPlugin = {
        name: 'Shockwave Flash',
        description: 'Shockwave Flash ' + FLASH_VERSION_MAJOR + '.0 r0',
        filename: 'pepflashplayer.dll',
        length: 1
    };
    var flashMime = makeMimeType(
        'application/x-shockwave-flash',
        'swf',
        'Shockwave Flash',
        flashPlugin
    );
    flashPlugin[0] = flashMime;
    flashPlugin.item = function (i) { return i === 0 ? flashMime : null; };
    flashPlugin.namedItem = function (n) {
        return n === 'application/x-shockwave-flash' ? flashMime : null;
    };

    var futureSplash = {
        name: 'Shockwave Flash',
        description: 'Shockwave Flash ' + FLASH_VERSION_MAJOR + '.0 r0',
        filename: 'pepflashplayer.dll',
        length: 0,
        item: function () { return null; },
        namedItem: function () { return null; }
    };

    // 伪造成 PluginArray 的样子。
    //
    // 关键点：原生 PluginArray 支持「按名字索引」——
    //   navigator.plugins['Shockwave Flash']
    // 这在浏览器里走的是 WebIDL 的 named getter，而普通 JS 对象
    // 必须显式挂上同名属性，否则 `plugins['Shockwave Flash']` 会返回 undefined，
    // 页面的 detect_flash() 依旧判定为「未安装 Flash」。
    var pluginArray = {
        0: flashPlugin,
        1: futureSplash,
        length: 2,
        item: function (i) { return this[i] || null; },
        namedItem: function (n) { return this[n] || null; },
        refresh: function () {}
    };
    // 按名字直取（detect_flash 用的就是这条路径）
    pluginArray['Shockwave Flash'] = flashPlugin;

    var mimeTypeArray = {
        0: flashMime,
        length: 1,
        item: function (i) { return this[i] || null; },
        namedItem: function (n) { return this[n] || null; }
    };
    mimeTypeArray['application/x-shockwave-flash'] = flashMime;

    function defineGetter(obj, prop, getter) {
        try {
            Object.defineProperty(obj, prop, {
                get: getter,
                configurable: true,
                enumerable: true
            });
            return true;
        } catch (e) {
            return false;
        }
    }

    // 覆盖 navigator.plugins / navigator.mimeTypes。
    // 注意 Chromium 中这两个属性定义在 Navigator.prototype 上，
    // 因此要在 navigator 实例自身、以及原型上都尝试定义。
    function install() {
        var targets = [navigator];
        try {
            if (Object.getPrototypeOf(navigator)) {
                targets.push(Object.getPrototypeOf(navigator));
            }
        } catch (e) { /* ignore */ }

        var okPlugins = false;
        for (var i = 0; i < targets.length && !okPlugins; i++) {
            okPlugins = defineGetter(targets[i], 'plugins', function () { return pluginArray; });
        }
        for (var j = 0; j < targets.length; j++) {
            defineGetter(targets[j], 'mimeTypes', function () { return mimeTypeArray; });
        }
        return okPlugins;
    }

    install();

    // 部分页面还会用 ActiveXObject 的存在性判断（老 IE 分支），
    // 这里提供一个"存在但会抛错"的桩，让 isIE 分支保持 false、
    // 从而走到 navigator.plugins 分支（也就是我们伪造好的那条）。
    // 不提供 ActiveXObject，保持现代浏览器语义。

    // 兜底：个别脚本会在 DOMContentLoaded 之后重新检查，
    // 再确保一次，防止被其它库覆盖。
    document.addEventListener('DOMContentLoaded', function () {
        try {
            if (!navigator.plugins || !navigator.plugins['Shockwave Flash']) {
                install();
            }
        } catch (e) { /* ignore */ }
    });
})();
