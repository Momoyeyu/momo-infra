<script setup lang="ts">
import { withBase } from 'vitepress'

const stages = [
  { name: '训练 pipeline', tag: 'LLM 后训练 · ASR / TTS 全训练', gate: false },
  { name: 'frozen eval gate', tag: '盲听 A / B · 回归题集', gate: true },
  { name: 'serving 底座', tag: 'vLLM 等开源实现', gate: false },
  { name: '统一 gateway', tag: '容灾 · 负载均衡 · 单一接口', gate: false },
]

const stories = [
  {
    number: '01',
    title: '训练',
    heading: '自有模型的训练 → 评测 → 上线。',
    body: 'LLM 后训练与 ASR/TTS 全训练 pipeline 由 momoctl 编排，本地与 SSH 远端执行，产物带着 manifest 与 checksum 回来。',
  },
  {
    number: '02',
    title: '守门',
    heading: '不达标，不 promote。',
    body: 'frozen eval gate 守门：声线走盲听 A/B，persona 走回归题集。activate 前先 probe，失败就保留旧版本。',
  },
  {
    number: '03',
    title: '出口',
    heading: '对外，永远只有一个入口。',
    body: '推理底座复用 vLLM 等开源实现；自研收敛到统一 gateway——对内多后端容灾与负载均衡，对外单一 OpenAI-compatible 接口。',
  },
]

const blueprintLinks = [
  ['定位与边界', '/guide/overview'],
  ['infra 控制面', '/guide/momo-infra'],
  ['配置规范', '/guide/config'],
  ['部署手册', '/guide/deployment'],
]
</script>

<template>
  <main class="infra-home">
    <section class="infra-hero" aria-labelledby="infra-title">
      <div class="infra-intro">
        <p class="eyebrow">SELF-HOSTED · AI INFRASTRUCTURE</p>
        <h1 id="infra-title"><span>模型从训练到上线，</span><span>走自己的 <em>momo-infra</em>。</span></h1>
        <p class="hero-copy">自有 AI 基础设施：训练 pipeline、serving 与统一 gateway。</p>
        <p class="hero-detail">不手搓推理引擎，不建私有协议。<br>复用开源底座，把自研收敛到一个稳定入口。</p>
        <div class="hero-links"><a :href="withBase('/guide/')">认识 momo-infra <span aria-hidden="true">→</span></a><a :href="withBase('/guide/overview')">打开定位与边界</a></div>
        <p class="build-note">正在构建：训练 → 评测 → serving 闭环，统一对外 gateway。</p>
      </div>
      <div class="infra-scene">
        <div class="scene-orbit" aria-hidden="true"></div>
        <div class="scene-ground" aria-hidden="true"></div>
        <ol class="scene-pipeline" aria-label="训练到 gateway 的链路">
          <li v-for="stage in stages" :key="stage.name" :class="{ 'stage-gate': stage.gate }">
            <span class="stage-name">{{ stage.name }}</span>
            <span class="stage-tag">{{ stage.tag }}</span>
          </li>
        </ol>
        <p class="scene-note">一次 promote，只走 gate</p>
        <p class="scene-caption">momo-infra · pipeline 示意</p>
      </div>
    </section>

    <section class="infra-story" aria-labelledby="story-title">
      <div class="story-heading">
        <div>
          <p class="eyebrow">01 / 闭环，而不只是跑起来</p>
          <h2 id="story-title">能训，能评，也能收得住。</h2>
        </div>
        <svg class="story-orbit" viewBox="0 0 280 120" aria-hidden="true">
          <ellipse cx="140" cy="60" rx="118" ry="34" />
          <circle cx="224" cy="36" r="5" />
          <path d="M40 72c28-18 53-18 78 0s50 18 75 0 47-18 67-2" />
        </svg>
      </div>
      <div class="story-rows">
        <article v-for="story in stories" :key="story.number" class="story-row">
          <p class="story-number">{{ story.number }}</p>
          <h3>{{ story.title }}</h3>
          <div><strong>{{ story.heading }}</strong><p>{{ story.body }}</p></div>
        </article>
      </div>
      <svg class="story-wave" viewBox="0 0 840 88" aria-hidden="true">
        <path d="M0 44h82l18-24 24 50 28-54 26 28h98l20-16 20 31 22-41 22 26h112l18-26 25 52 29-48 20 22h145" />
      </svg>
    </section>

    <section class="infra-blueprint" aria-labelledby="blueprint-title">
      <div class="blueprint-copy">
        <p class="eyebrow">02 / BLUEPRINT</p>
        <h2 id="blueprint-title"><span>把基础设施拆成</span><span>可以验收的边界。</span></h2>
        <p>编排不进数据通路，配置只走 env，产物只认校验过的 manifest。每一层都能替换，也都不越权。</p>
      </div>
      <div class="blueprint-flow" aria-label="训练 pipeline 到统一 gateway 的三段链路">
        <span>训练 pipeline</span><i aria-hidden="true"></i><span>serving 底座</span><i aria-hidden="true"></i><span>统一 gateway</span>
      </div>
      <nav class="blueprint-links" aria-label="设计文档入口">
        <a v-for="([label, link], index) in blueprintLinks" :key="link" :href="withBase(link)"><span>{{ String(index + 1).padStart(2, '0') }}</span>{{ label }}<b aria-hidden="true">↗</b></a>
      </nav>
    </section>

    <section class="infra-road" aria-labelledby="road-title">
      <div>
        <p class="eyebrow">03 / ROAD</p>
        <h2 id="road-title">先把运维握在手里，再让 gateway 替所有后端说话。</h2>
      </div>
      <ol>
        <li><span>当前</span><strong>momoctl 基础运维</strong><p>doctor / pull / serve / switch / bench / gc，CLI 与 Web 控制台共用同一核心。</p></li>
        <li><span>随后</span><strong>训练编排</strong><p>jobs 状态机 + pipeline 定义，本地与 SSH 远端执行，checkpoint 可续训，产物可回滚。</p></li>
        <li><span>之后</span><strong>统一 gateway</strong><p>对内多模型容灾与负载均衡，对外单一接口；ASR/TTS 接口对齐主流云服务。</p></li>
      </ol>
      <div class="road-end">
        <a :href="withBase('/guide/momo-infra')">阅读 infra 设计 <span aria-hidden="true">→</span></a>
        <p>终端产品是 momo：app 只持有一个 endpoint，容灾永远在服务侧。</p>
      </div>
    </section>

    <footer class="infra-home-footer">
      <span>momo-infra · 自有 AI 基础设施</span>
      <a :href="withBase('/guide/docs-index')">全部设计文档 ↗</a>
    </footer>
  </main>
</template>
