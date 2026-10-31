import { createApp } from 'vue';
import App from './App.vue';
import './style.css';
// 挂载前应用持久化的主题，避免首帧闪色
import './services/theme';

createApp(App).mount('#app');
