package flash.system {
    [Ruffle(Abstract)]
    public final class System {
        import __ruffle__.stub_method;
        import __ruffle__.stub_getter;

        // 原本是空实现：游戏在切场景后主动调 System.gc() 想清理，
        // 在 Ruffle 里等于什么都没做。改为真正触发一轮 GC。
        public static native function gc():void;

        public static function pauseForGCIfCollectionImminent(imminence:Number = 0.75):void {
            stub_method("flash.system.System", "pauseForGCIfCollectionImminent");
        }

        public static native function setClipboard(string:String):void;

        public static function disposeXML(node:XML):void {
            stub_method("flash.system.System", "disposeXML");
        }

        public static native function exit(code:uint):void;

        // 这三个值原本是写死的假数据（90MB / 100MB / 10MB）。
        // 页游会用它们做资源缓存的自适应：读到"只用了 90MB"就一直往里塞，
        // 于是内存一路涨。改为返回真实的进程 RSS（见 system.rs）。
        public static native function get freeMemory():Number;

        public static native function get privateMemory():Number;

        public static native function get totalMemoryNumber():Number;

        public static function get totalMemory():uint {
            return totalMemoryNumber as uint;
        }
    }
}
