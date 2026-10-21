package flash.display {
    import __ruffle__.stub_method;

    import flash.display.LoaderInfo;
    import flash.display.DisplayObject;
    import flash.errors.IllegalOperationError;
    import flash.system.LoaderContext;
    import flash.utils.ByteArray;
    import flash.net.URLRequest;
    import flash.events.UncaughtErrorEvents;
    import flash.display.MovieClip;
    import flash.system.System;

    [Ruffle(InstanceAllocator)]
    public class Loader extends DisplayObjectContainer {

        [Ruffle(NativeAccessible)]
        private var _contentLoaderInfo:LoaderInfo;

        public function get contentLoaderInfo():LoaderInfo {
            return this._contentLoaderInfo;
        }

        public function get content():DisplayObject {
            return this._contentLoaderInfo.content;
        }

        public native function load(request:URLRequest, context:LoaderContext = null):void;

        public native function loadBytes(data:ByteArray, context:LoaderContext = null):void;

        public native function unload():void;

        [API("662")]
        public function unloadAndStop(gc:Boolean = true):void {
            // 内测魔改版：补全语义。
            //
            // Flash 的 unloadAndStop 除了卸载内容，还会：
            //   1) 停止内容里所有时间轴（否则已卸载的 MovieClip 仍在跑 enterFrame）；
            //   2) 断开事件监听；
            //   3) 视 gc 参数提示回收。
            //
            // Ruffle 原本只调了 unload()，导致战报/场景切换后，
            // 旧内容的时间轴仍在推进、持续产生对象，长挂机时内存只增不减。
            //
            // 这里先停顶层时间轴（递归遍历几万个显示对象代价过高，
            // 顶层停止后子对象随父级一起停摆，已能覆盖绝大多数场景），
            // 再执行卸载，最后按参数触发一次回收。
            var content:DisplayObject = this.content;
            if (content != null && content is MovieClip) {
                (content as MovieClip).stop();
            }
            this.unload();
            if (gc) {
                System.gc();
            }
        }

        public function close():void {
            stub_method("flash.display.Loader", "close");
        }

        override public function addChild(child:DisplayObject):DisplayObject {
            throw new IllegalOperationError("Error #2069: The Loader class does not implement this method.", 2069);
        }

        override public function addChildAt(child:DisplayObject, index:int):DisplayObject {
            throw new IllegalOperationError("Error #2069: The Loader class does not implement this method.", 2069);
        }

        override public function removeChild(child:DisplayObject):DisplayObject {
            throw new IllegalOperationError("Error #2069: The Loader class does not implement this method.", 2069);
        }

        override public function removeChildAt(index:int):DisplayObject {
            throw new IllegalOperationError("Error #2069: The Loader class does not implement this method.", 2069);
        }

        override public function setChildIndex(child:DisplayObject, index:int):void {
            throw new IllegalOperationError("Error #2069: The Loader class does not implement this method.", 2069);
        }

        [API("667")]
        public function get uncaughtErrorEvents():UncaughtErrorEvents {
            return this.contentLoaderInfo.uncaughtErrorEvents;
        }
    }
}
