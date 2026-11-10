/* wr.c — 基于官方 wbf 二维 LUT 的窗口化灰度渲染器
 *
 * 核心：idx = cur*32 + prev（state = 2*gray），code 1=推黑 2=推白 0=不动
 * 窗口：setmode(334, h, 1405) + pan(y0) —— 只扫 h 行，时间 ∝ h
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <fcntl.h>
#include <unistd.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <linux/fb.h>
#include <time.h>
#include "zm334.h"
#include "wbf_lut.h"

#define ROWS    1405
#define PWID    1872
#define LINE    (CELLS334*4)
#define FRAME   (LINE*ROWS)

static unsigned char *fb; static int fd;
static unsigned char *prev_g, *cur_g;      /* 4bit 灰阶 0..15 */
static unsigned int   rowctl[2][CELLS334];
static double ms(void){struct timespec t;clock_gettime(CLOCK_MONOTONIC,&t);return t.tv_sec*1e3+t.tv_nsec/1e6;}

static int setmode(int x,int y,int yv,int pixc){
    struct fb_var_screeninfo v;
    if(ioctl(fd,FBIOGET_VSCREENINFO,&v))return -1;
    v.xres=x;v.yres=y;v.xres_virtual=x;v.yres_virtual=yv;
    v.pixclock=pixc;v.left_margin=1;v.right_margin=1;
    v.upper_margin=1;v.lower_margin=1;v.hsync_len=1;v.vsync_len=1;
    v.activate=0x80000000|FB_ACTIVATE_NOW;
    return ioctl(fd,FBIOPUT_VSCREENINFO,&v);
}
static int pan(int yoff){struct fb_var_screeninfo v;if(ioctl(fd,FBIOGET_VSCREENINFO,&v))return -99;v.yoffset=yoff;return ioctl(fd,FBIOPAN_DISPLAY,&v);}
static int gpu(void){return 0;}
static unsigned char *G(int y,int x){ return cur_g + (size_t)y*PWID + x; }
static unsigned char *P(int y,int x){ return prev_g + (size_t)y*PWID + x; }

/* 把 256 字节 LUT 展开成 T[gc*16+gp] */
static void prep(const unsigned char src[256], unsigned char T[256]){
    for(int gp=0;gp<16;gp++) for(int gc=0;gc<16;gc++){
        int idx = ((gc*2)<<5) | (gp*2);       /* cur*32 + prev */
        int b = src[idx>>2];
        T[(gc<<4)|gp] = (b >> ((idx&3)*2)) & 3;
    }
}
/* 用某个模式渲染窗口 [y0, y0+h) × 全宽 */
static unsigned char *scratch;   /* h*LINE 的临时帧 */

/* 全屏闪白：code2 x24，把面板拉到确定白底 */
static void flash_white(void){
    setmode(CELLS334,ROWS,ROWS,25000);
    memcpy(scratch,rowctl[0],LINE);
    for(int y=1;y<ROWS;y++) memcpy(scratch+(size_t)y*LINE,rowctl[1],LINE);
    { unsigned int *cells=(unsigned int*)scratch;
      for(int y=0;y<ROWS;y++){ unsigned int *r=cells+(size_t)y*CELLS334;
        for(int c=0;c<234;c++) r[CX0+c]|=0xAAAAu; } }
    for(int i=0;i<24;i++){ memcpy(fb,scratch,FRAME); pan(0); }
}
/* 收尾：恢复全屏 + 翻一帧全 0（不驱动） */
static void finish(void){
    setmode(CELLS334,ROWS,ROWS,25000);
    memcpy(scratch,rowctl[0],LINE);
    for(int y=1;y<ROWS;y++) memcpy(scratch+(size_t)y*LINE,rowctl[1],LINE);
    memcpy(fb,scratch,FRAME);
    pan(0);
}
/* 渲染窗口 [y0, y0+h) × 全宽：先把 nph 帧全建到 RAM，再背靠背 pan
 * 这样每个相位停留正好 1 个面板帧，不会被编码时间拉长 */
static double render_win_px(const unsigned char (*lut)[256],int nph,int y0,int h,int pixc);
static double render_win(const unsigned char (*lut)[256],int nph,int y0,int h){
    return render_win_px(lut,nph,y0,h,25000);
}
static double render_win_px(const unsigned char (*lut)[256],int nph,int y0,int h,int pixc){
    double t0=ms();
    size_t fsz=(size_t)h*LINE;
    unsigned char *bufs=malloc(fsz*nph);
    if(!bufs){fprintf(stderr,"nomem bufs %d\n",h);return -1;}
    unsigned char T[256];
    for(int ph=0;ph<nph;ph++){
        prep(lut[ph],T);
        unsigned char *fbuf=bufs+(size_t)ph*fsz;
        unsigned int *cells=(unsigned int*)fbuf;
        for(int y=0;y<h;y++){
            int yy=y0+y;
            unsigned int *row=cells+(size_t)y*CELLS334;
            memcpy(row,rowctl[yy<1?0:1],LINE);
            const unsigned char *cg=cur_g+(size_t)yy*PWID;
            const unsigned char *pg=prev_g+(size_t)yy*PWID;
            unsigned int *dst=row+CX0;
            for(int c=0;c<234;c++){
                unsigned int pk =
                    ((unsigned int)T[(cg[0]<<4)|pg[0]]<<14) |
                    ((unsigned int)T[(cg[1]<<4)|pg[1]]<<12) |
                    ((unsigned int)T[(cg[2]<<4)|pg[2]]<<10) |
                    ((unsigned int)T[(cg[3]<<4)|pg[3]]<< 8) |
                    ((unsigned int)T[(cg[4]<<4)|pg[4]]<< 6) |
                    ((unsigned int)T[(cg[5]<<4)|pg[5]]<< 4) |
                    ((unsigned int)T[(cg[6]<<4)|pg[6]]<< 2) |
                    ((unsigned int)T[(cg[7]<<4)|pg[7]]);
                *dst++ |= pk;
                cg+=8; pg+=8;
            }
        }
    }
    double t1=ms();
    if(setmode(CELLS334,h,ROWS,pixc)) perror("setmode win");
    for(int ph=0;ph<nph;ph++){
        memcpy(fb+(size_t)y0*LINE,bufs+(size_t)ph*fsz,fsz);
        int r=pan(y0);
        if(ph==0) printf("  pan ret=%d\n",r);
    }
    double t2=ms();
    free(bufs);
    finish();
    printf("  构建 %.1f ms | 播放 %.1f ms (%.2f ms/相位)\n",t1-t0,t2-t1,(t2-t1)/nph);
    return t2-t0;
}
static void sync_prev(void){ memcpy(prev_g,cur_g,(size_t)ROWS*PWID); }

/* ------------ 测试图样 ------------ */
static void draw_full_page(void){
    for(int y=0;y<ROWS;y++) memset(G(y,0),15,PWID);
    for(int y=40;y<170;y++) memset(G(y,0),0,PWID);
    for(int y=70;y<140;y++) for(int k=0;k<7;k++){int x0=90+k*250;memset(G(y,x0),15,150);}
    int y0=240;
    for(int ln=0;ln<30;ln++){
        int y=y0+ln*37; if(y+22>=ROWS)break;
        int len=300+((ln*137)%1100);
        for(int yy=y;yy<y+22;yy++) memset(G(yy,120),0,len);
    }
}
static void draw_grad_band(int y0,int h){
    for(int y=y0;y<y0+h;y++) for(int x=0;x<PWID;x++) G(y,x)[0]=(unsigned char)((x*16)/PWID);
    /* 叠一个圆形渐变块，检验二维图案 */
    int cx=1400, cy=y0+h/2, r=h/2-10;
    for(int y=y0;y<y0+h;y++) for(int x=cx-r;x<cx+r;x++){
        int dx=x-cx, dy=y-cy;
        if(dx*dx+dy*dy < r*r) G(y,x)[0]=(unsigned char)((dx*dx+dy*dy)*16/(r*r));
    }
}

int main(int argc,char**argv){
    const char *st=argc>1?argv[1]:"demo";
    fd=open("/dev/fb0",O_RDWR); if(fd<0){perror("open");return 1;}
    fb=mmap(0,FRAME,PROT_READ|PROT_WRITE,MAP_SHARED,fd,0);
    if(fb==MAP_FAILED){perror("mmap");return 1;}
    for(int k=0;k<2;k++) for(int c=0;c<CELLS334;c++) rowctl[k][c]=(unsigned int)tmpl[k][c]<<16;
    prev_g=malloc((size_t)ROWS*PWID); cur_g=malloc((size_t)ROWS*PWID);
    if(!prev_g||!cur_g){fprintf(stderr,"nomem\n");return 1;}
    memset(prev_g,15,(size_t)ROWS*PWID); memset(cur_g,15,(size_t)ROWS*PWID);

    scratch=malloc((size_t)ROWS*LINE);
    if(!scratch){fprintf(stderr,"nomem scratch\n");return 1;}

    if(!strcmp(st,"demo")){
        /* 0) 建立确定的白色基准 */
        memset(prev_g,15,(size_t)ROWS*PWID);
        memset(cur_g,15,(size_t)ROWS*PWID);
        double t0=ms(); flash_white(); printf("闪白 24 帧: %.1f ms\n",ms()-t0);
        /* 1) 全屏 GC16 画一页 */
        draw_full_page();
        double t1=render_win(lut_GC16,NPH_GC16,0,ROWS);
        printf("全屏 GC16 46 相位: %.1f ms  (%.2f ms/相位)\n",t1,t1/NPH_GC16);
        sync_prev();
        /* 2) 窗口内灰度：y0=600 高 200 */
        draw_grad_band(600,200);
        double t2=render_win(lut_GC16,NPH_GC16,600,200);
        printf("窗口 200 行 GC16 16级: %.1f ms  (%.2f ms/相位)\n",t2,t2/NPH_GC16);
        sync_prev();
        /* 3) 窗口内 A2 二值 */
        for(int y=900;y<1000;y++) for(int x=0;x<PWID;x++) G(y,x)[0]=((x/60)&1)?0:15;
        double t3=render_win(lut_A2,NPH_A2,900,100);
        printf("窗口 100 行 A2 二值:   %.1f ms  (%.2f ms/相位)\n",t3,t3/NPH_A2);
        sync_prev();
        return 0;
    }
    if(!strcmp(st,"full")){
        memset(prev_g,15,(size_t)ROWS*PWID);
        memset(cur_g,15,(size_t)ROWS*PWID);
        flash_white(); printf("闪白完成\n");
        draw_full_page();
        double t1=render_win(lut_GC16,NPH_GC16,0,ROWS);
        printf("全屏 GC16: %.1f ms\n",t1);
        return 0;
    }
    if(!strcmp(st,"clock")){
        memset(prev_g,15,(size_t)ROWS*PWID);
        memset(cur_g,15,(size_t)ROWS*PWID);
        flash_white(); printf("闪白完成\n");
        draw_full_page();
        int pcs[]={25000,15000,10000,7000,5000};
        for(int i=0;i<5;i++){
            memset(prev_g,15,(size_t)ROWS*PWID);
            setmode(CELLS334,ROWS,ROWS,25000);
            /* 每轮先回到白底，保证对比公平 */
            memcpy(scratch,rowctl[0],LINE);
            for(int y=1;y<ROWS;y++) memcpy(scratch+(size_t)y*LINE,rowctl[1],LINE);
            { unsigned int *cl=(unsigned int*)scratch;
              for(int y=0;y<ROWS;y++){ unsigned int *r=cl+(size_t)y*CELLS334;
                for(int c=0;c<234;c++) r[CX0+c]|=0xAAAAu; } }
            setmode(CELLS334,ROWS,ROWS,25000);
            { unsigned char *b=malloc((size_t)FRAME); void *keep=b; (void)keep; free(b); }
            flash_white();
            double t=render_win_px(lut_GC16,NPH_GC16,0,ROWS,pcs[i]);
            printf("pixclock=%-6d 全屏 GC16: %.0f ms\n",pcs[i],t);
        }
        /* 最后留一张 5000 的页面给眼睛看 */
        memset(prev_g,15,(size_t)ROWS*PWID);
        flash_white();
        draw_full_page();
        render_win_px(lut_GC16,NPH_GC16,0,ROWS,5000);
        printf("已用 pixclock=5000 渲染最终页面\n");
        return 0;
    }
    if(!strcmp(st,"wdiag")){
        /* 窗口内放 4 条 50 行交替黑白带，其余 code 0；看屏上出现几条、在哪 */
        memset(prev_g,15,(size_t)ROWS*PWID);
        memset(cur_g,15,(size_t)ROWS*PWID);
        flash_white(); printf("闪白完成\n");
        for(int y=600;y<800;y++){
            int band=(y-600)/50;                 /* 0..3 */
            unsigned char g = (band&1)?15:0;     /* 黑 白 黑 白 */
            memset(cur_g+(size_t)y*PWID,g,PWID);
        }
        render_win(lut_GC16,NPH_GC16,600,200);
        printf("诊断带完成\n");
        return 0;
    }
    if(!strcmp(st,"w1")){
        memset(prev_g,15,(size_t)ROWS*PWID);
        memset(cur_g,15,(size_t)ROWS*PWID);
        flash_white(); printf("闪白完成\n");
        draw_full_page();
        render_win(lut_GC16,NPH_GC16,0,ROWS);
        sync_prev();
        printf("全屏页完成，准备窗口\n");
        draw_grad_band(600,200);
        double t=render_win(lut_GC16,NPH_GC16,600,200);
        printf("窗口完成 %.1f ms\n",t);
        return 0;
    }
    if(!strcmp(st,"bench")){
        draw_grad_band(600,200);
        for(int k=0;k<5;k++){
            double t=render_win(lut_GC16,NPH_GC16,600,200);
            printf("GC16 200行 第%d次: %.1f ms\n",k,t);
        }
        for(int k=0;k<5;k++){
            double t=render_win(lut_A2,NPH_A2,600,200);
            printf("A2   200行 第%d次: %.1f ms\n",k,t);
        }
        return 0;
    }
    fprintf(stderr,"usage: wr demo|full|bench\n");
    return 1;
}
