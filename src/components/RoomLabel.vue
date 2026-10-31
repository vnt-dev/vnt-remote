<script setup lang="ts">
import { useId } from 'vue';
const tooltipId = useId();
</script>

<template>
  <span class="room-label" tabindex="0" :aria-describedby="tooltipId">
    房间编号<span class="help-marker" aria-hidden="true">?</span>
    <span :id="tooltipId" class="room-tooltip" role="tooltip"
      >相同服务器相同编号的设备可以相互发现</span
    >
  </span>
</template>

<style scoped>
.room-label {
  position: relative;
  display: inline-flex;
  align-items: center;
  gap: 5px;
  width: fit-content;
  cursor: help;
}
.help-marker {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 14px;
  height: 14px;
  border: 1px solid var(--text-3);
  border-radius: 50%;
  color: var(--text-3);
  font-size: 10px;
  font-weight: 400;
  line-height: 1;
}
.room-tooltip {
  position: absolute;
  z-index: 20;
  bottom: calc(100% + 9px);
  left: 0;
  width: 240px;
  max-width: calc(100vw - 80px);
  padding: 9px 12px;
  border: 1px solid var(--border-strong);
  border-radius: 8px;
  background: var(--surface);
  color: var(--text);
  box-shadow: var(--shadow-pop);
  font-size: 12px;
  font-weight: 400;
  line-height: 1.6;
  white-space: normal;
  visibility: hidden;
  opacity: 0;
  pointer-events: none;
}
.room-label:hover .room-tooltip,
.room-label:focus .room-tooltip {
  visibility: visible;
  opacity: 1;
}
</style>
